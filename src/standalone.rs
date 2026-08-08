//! PreVocal standalone application (desktop).
//!
//! This module runs the same DSP as the plugin as a native application using Slint for
//! the GUI and cpal for realtime audio I/O, so it can be tested without a DAW on both
//! Windows and Linux. The DSP lives in `PreVocal::PreVocalDsp` and is shared with the
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
//! The toolbar lets you pick the audio driver (e.g. ASIO or WASAPI) and, below it,
//! the output and input devices of that driver independently. Restart and rescan
//! buttons are provided, and the last selection is persisted to a small file so it
//! is restored on the next launch.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use nice_plug::params::InternalParamMut;
use nice_plug::prelude::*;
use prevocal::{level_to_meter, preset_names, MeterState, PreVocalDsp, PreVocalParams, Preset, PRESETS};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

slint::include_modules!();

// ---------------------------------------------------------------------
// Parameter bridge. The Slint UI talks to the same `Arc<PreVocalParams>`
// the audio thread reads from, through raw [`ParamPtr`]s.
// ---------------------------------------------------------------------

struct UiBridge {
    params: Arc<PreVocalParams>,
    /// Current audio sample rate, written by `AudioManager` on start. Needed to
    /// re-arm the parameter smoothers after a UI edit, exactly like the
    /// standalone wrapper in nice-plug does.
    sample_rate: Arc<AtomicU32>,
}

impl UiBridge {
    fn new(params: &Arc<PreVocalParams>, sample_rate: Arc<AtomicU32>) -> Self {
        Self {
            params: Arc::clone(params),
            sample_rate,
        }
    }

    // The DSP smoothers only pick up a new target after
    // `_internal_update_smoother` is called (mirrors the standalone host in
    // nice-plug). Without this the faders/knobs would only "apply" on a stream
    // restart, so we re-arm the smoothed params on every UI write.
    fn update_smoothers(&self) {
        let sample_rate = self.sample_rate.load(Ordering::Relaxed);
        if sample_rate == 0 {
            return;
        }
        let sample_rate = sample_rate as f32;
        unsafe {
            self.params
                .drive
                ._internal_update_smoother(sample_rate, false);
            self.params
                .hpf
                ._internal_update_smoother(sample_rate, false);
            self.params
                .lpf
                ._internal_update_smoother(sample_rate, false);
            self.params
                .air
                ._internal_update_smoother(sample_rate, false);
            self.params
                .tube_character
                ._internal_update_smoother(sample_rate, false);
            self.params
                .tube_sag
                ._internal_update_smoother(sample_rate, false);
            self.params
                .comp_thresh
                ._internal_update_smoother(sample_rate, false);
            self.params
                .comp_ratio
                ._internal_update_smoother(sample_rate, false);
            self.params
                .comp_attack
                ._internal_update_smoother(sample_rate, false);
            self.params
                .comp_release
                ._internal_update_smoother(sample_rate, false);
            self.params
                .comp_makeup
                ._internal_update_smoother(sample_rate, false);
            self.params
                .delay_time
                ._internal_update_smoother(sample_rate, false);
            self.params
                .delay_feedback
                ._internal_update_smoother(sample_rate, false);
            self.params
                .delay_mix
                ._internal_update_smoother(sample_rate, false);
            self.params
                .reverb_size
                ._internal_update_smoother(sample_rate, false);
            self.params
                .reverb_damping
                ._internal_update_smoother(sample_rate, false);
            self.params
                .reverb_mix
                ._internal_update_smoother(sample_rate, false);
            self.params
                .output_trim
                ._internal_update_smoother(sample_rate, false);
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
        self.update_smoothers();
    }

    fn write_hpf(&self, hz: f32) {
        let hz = hz.clamp(20.0, 200.0);
        if !hz.is_finite() {
            return;
        }
        unsafe {
            self.params.hpf._internal_set_plain_value(hz);
        }
        self.update_smoothers();
    }

    fn write_lpf(&self, hz: f32) {
        let hz = hz.clamp(500.0, 20_000.0);
        if !hz.is_finite() {
            return;
        }
        unsafe {
            self.params.lpf._internal_set_plain_value(hz);
        }
        self.update_smoothers();
    }

    fn write_air(&self, db: f32) {
        let db = db.clamp(0.0, 6.0);
        if !db.is_finite() {
            return;
        }
        unsafe {
            self.params.air._internal_set_plain_value(db);
        }
        self.update_smoothers();
    }

    fn write_tube_character(&self, v: f32) {
        let v = v.clamp(0.0, 1.0);
        if !v.is_finite() {
            return;
        }
        unsafe {
            self.params.tube_character._internal_set_plain_value(v);
        }
        self.update_smoothers();
    }

    fn write_tube_sag(&self, v: f32) {
        let v = v.clamp(0.0, 1.0);
        if !v.is_finite() {
            return;
        }
        unsafe {
            self.params.tube_sag._internal_set_plain_value(v);
        }
        self.update_smoothers();
    }

    fn write_comp_thresh(&self, db: f32) {
        let db = db.clamp(-60.0, 0.0);
        if !db.is_finite() {
            return;
        }
        unsafe {
            self.params.comp_thresh._internal_set_plain_value(db);
        }
        self.update_smoothers();
    }

    fn write_comp_ratio(&self, ratio: f32) {
        let ratio = ratio.clamp(1.0, 20.0);
        if !ratio.is_finite() {
            return;
        }
        unsafe {
            self.params.comp_ratio._internal_set_plain_value(ratio);
        }
        self.update_smoothers();
    }

    fn write_comp_attack(&self, ms: f32) {
        let ms = ms.clamp(0.1, 100.0);
        if !ms.is_finite() {
            return;
        }
        unsafe {
            self.params.comp_attack._internal_set_plain_value(ms);
        }
        self.update_smoothers();
    }

    fn write_comp_release(&self, ms: f32) {
        let ms = ms.clamp(10.0, 1_000.0);
        if !ms.is_finite() {
            return;
        }
        unsafe {
            self.params.comp_release._internal_set_plain_value(ms);
        }
        self.update_smoothers();
    }

    fn write_comp_makeup(&self, db: f32) {
        let db = db.clamp(0.0, 24.0);
        if !db.is_finite() {
            return;
        }
        let gain = util::db_to_gain(db);
        unsafe {
            self.params.comp_makeup._internal_set_plain_value(gain);
        }
        self.update_smoothers();
    }

    fn write_delay_time(&self, ms: f32) {
        let ms = ms.clamp(1.0, 1_000.0);
        if !ms.is_finite() {
            return;
        }
        unsafe {
            self.params.delay_time._internal_set_plain_value(ms);
        }
        self.update_smoothers();
    }

    fn write_delay_feedback(&self, pct: f32) {
        let pct = pct.clamp(0.0, 90.0);
        if !pct.is_finite() {
            return;
        }
        unsafe {
            self.params.delay_feedback._internal_set_plain_value(pct);
        }
        self.update_smoothers();
    }

    fn write_delay_mix(&self, pct: f32) {
        let pct = pct.clamp(0.0, 100.0);
        if !pct.is_finite() {
            return;
        }
        unsafe {
            self.params.delay_mix._internal_set_plain_value(pct);
        }
        self.update_smoothers();
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
        self.update_smoothers();
    }

    fn write_reverb_size(&self, size: f32) {
        let size = size.clamp(0.0, 1.0);
        if !size.is_finite() {
            return;
        }
        unsafe {
            self.params.reverb_size._internal_set_plain_value(size);
        }
        self.update_smoothers();
    }

    fn write_reverb_damping(&self, damping: f32) {
        let damping = damping.clamp(0.0, 1.0);
        if !damping.is_finite() {
            return;
        }
        unsafe {
            self.params
                .reverb_damping
                ._internal_set_plain_value(damping);
        }
        self.update_smoothers();
    }

    fn write_reverb_mix(&self, pct: f32) {
        let pct = pct.clamp(0.0, 100.0);
        if !pct.is_finite() {
            return;
        }
        unsafe {
            self.params.reverb_mix._internal_set_plain_value(pct);
        }
        self.update_smoothers();
    }

    /// Apply a factory preset to every parameter at once.
    fn apply_preset(&self, preset: &Preset) {
        self.write_drive(preset.drive_db);
        self.write_hpf(preset.hpf_hz);
        self.write_lpf(preset.lpf_hz);
        self.write_air(preset.air_db);
        self.write_tube_character(preset.tube_character);
        self.write_tube_sag(preset.tube_sag);
        self.write_comp_thresh(preset.comp_thresh_db);
        self.write_comp_ratio(preset.comp_ratio);
        self.write_comp_attack(preset.comp_attack_ms);
        self.write_comp_release(preset.comp_release_ms);
        self.write_comp_makeup(preset.comp_makeup_db);
        unsafe {
            self.params
                .comp_bypass
                ._internal_set_plain_value(preset.comp_bypass);
        }
        self.write_delay_time(preset.delay_time_ms);
        self.write_delay_feedback(preset.delay_feedback_pct);
        self.write_delay_mix(preset.delay_mix_pct);
        unsafe {
            self.params
                .delay_bypass
                ._internal_set_plain_value(preset.delay_bypass);
        }
        self.write_reverb_size(preset.reverb_size);
        self.write_reverb_damping(preset.reverb_damping);
        self.write_reverb_mix(preset.reverb_mix_pct);
        unsafe {
            self.params
                .reverb_bypass
                ._internal_set_plain_value(preset.reverb_bypass);
        }
        self.write_output_trim(preset.trim_db);
    }

    fn drive_value(&self) -> f32 {
        util::gain_to_db(self.params.drive.modulated_plain_value())
    }

    fn hpf_value(&self) -> f32 {
        self.params.hpf.modulated_plain_value()
    }

    fn lpf_value(&self) -> f32 {
        self.params.lpf.modulated_plain_value()
    }

    fn air_value(&self) -> f32 {
        self.params.air.modulated_plain_value()
    }

    fn tube_character_value(&self) -> f32 {
        self.params.tube_character.modulated_plain_value()
    }

    fn tube_sag_value(&self) -> f32 {
        self.params.tube_sag.modulated_plain_value()
    }

    fn comp_thresh_value(&self) -> f32 {
        self.params.comp_thresh.modulated_plain_value()
    }

    fn comp_ratio_value(&self) -> f32 {
        self.params.comp_ratio.modulated_plain_value()
    }

    fn comp_attack_value(&self) -> f32 {
        self.params.comp_attack.modulated_plain_value()
    }

    fn comp_release_value(&self) -> f32 {
        self.params.comp_release.modulated_plain_value()
    }

    fn comp_makeup_value(&self) -> f32 {
        util::gain_to_db(self.params.comp_makeup.modulated_plain_value())
    }

    fn delay_time_value(&self) -> f32 {
        self.params.delay_time.modulated_plain_value()
    }

    fn delay_feedback_value(&self) -> f32 {
        self.params.delay_feedback.modulated_plain_value()
    }

    fn delay_mix_value(&self) -> f32 {
        self.params.delay_mix.modulated_plain_value()
    }

    fn reverb_size_value(&self) -> f32 {
        self.params.reverb_size.modulated_plain_value()
    }

    fn reverb_damping_value(&self) -> f32 {
        self.params.reverb_damping.modulated_plain_value()
    }

    fn reverb_mix_value(&self) -> f32 {
        self.params.reverb_mix.modulated_plain_value()
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

// ---------------------------------------------------------------------
// Audio manager: enumerates hosts + devices, keeps the selected device
// alive and owns the cpal streams. The GUI talks to it through a Mutex.
// ---------------------------------------------------------------------

/// A single selectable device (input or output) of a given driver.
#[derive(Clone)]
struct DeviceEntry {
    device_name: String,
    device: cpal::Device,
}

/// A selectable driver in the toolbar. ASIO hosts expose one driver per device
/// (e.g. "Steinberg built-in ASIO Driver"), so each becomes its own entry; other
/// hosts (WASAPI) map to a single entry covering all of their devices.
#[derive(Clone)]
struct DriverEntry {
    host_id: cpal::HostId,
    /// For the ASIO host, the specific ASIO driver name; `None` for other hosts.
    asio_driver: Option<String>,
}

impl DriverEntry {
    fn label(&self) -> String {
        match &self.asio_driver {
            Some(name) => format!("{} — {name}", self.host_id.name()),
            None => self.host_id.name().to_string(),
        }
    }
}

struct AudioManager {
    params: Arc<PreVocalParams>,
    drivers: Vec<DriverEntry>,
    driver_labels: Vec<String>,
    driver_current: usize,
    input_devices: Vec<DeviceEntry>,
    input_labels: Vec<String>,
    input_current: usize,
    output_devices: Vec<DeviceEntry>,
    output_labels: Vec<String>,
    output_current: usize,
    /// Gates the audio callbacks (toggled on stop/start).
    active: Arc<AtomicBool>,
    /// Keeps the meter poll thread alive for the whole app lifetime.
    alive: Arc<AtomicBool>,
    meters: Arc<Mutex<MeterState>>,
    streams: Option<Vec<cpal::Stream>>,
    status: String,
    /// Shared with `UiBridge` so it can re-arm the smoothers at the right rate.
    sample_rate: Arc<AtomicU32>,
}

impl AudioManager {
    fn new(params: Arc<PreVocalParams>, sample_rate: Arc<AtomicU32>) -> Self {
        let drivers = Self::enum_drivers();
        let driver_labels = Self::make_driver_labels(&drivers);
        let mut mgr = Self {
            params,
            drivers,
            driver_labels,
            driver_current: 0,
            input_devices: Vec::new(),
            input_labels: Vec::new(),
            input_current: 0,
            output_devices: Vec::new(),
            output_labels: Vec::new(),
            output_current: 0,
            active: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(true)),
            meters: Arc::new(Mutex::new(MeterState::default())),
            streams: None,
            status: String::new(),
            sample_rate,
        };

        // Restore the last used driver + input/output devices if still present.
        let saved = Self::load_last_selection();
        if let Some((host_name, asio_name, _, _)) = &saved
            && let Some(idx) = mgr.drivers.iter().position(|d| {
                d.host_id.name().eq_ignore_ascii_case(host_name)
                    && d.asio_driver
                        .as_deref()
                        .map(|n| n.eq_ignore_ascii_case(asio_name))
                        .unwrap_or_else(|| asio_name.is_empty())
            })
        {
            mgr.driver_current = idx;
        }
        mgr.reload_devices();
        if let Some((_, _, input_name, output_name)) = &saved {
            if let Some(idx) = mgr
                .input_devices
                .iter()
                .position(|d| d.device_name.eq_ignore_ascii_case(input_name))
            {
                mgr.input_current = idx;
            }
            if let Some(idx) = mgr
                .output_devices
                .iter()
                .position(|d| d.device_name.eq_ignore_ascii_case(output_name))
            {
                mgr.output_current = idx;
            }
        }

        if let Err(e) = mgr.start() {
            tracing::error!("Audio start failed: {e}");
        }
        mgr
    }

    /// Whether a host id refers to the ASIO host. Only compiled when the
    /// `asio` feature is enabled; `cpal::HostId::Asio` doesn't exist otherwise.
    fn is_asio_host(host_id: cpal::HostId) -> bool {
        #[cfg(feature = "asio")]
        {
            return host_id == cpal::HostId::Asio;
        }
        #[cfg(not(feature = "asio"))]
        {
            let _ = host_id;
            false
        }
    }

    /// Enumerate the selectable drivers. ASIO contributes one entry per driver;
    /// every other host contributes a single entry.
    fn enum_drivers() -> Vec<DriverEntry> {
        let mut drivers = Vec::new();
        for host_id in cpal::available_hosts() {
            if Self::is_asio_host(host_id) {
                let Ok(host) = cpal::host_from_id(host_id) else {
                    continue;
                };
                let Ok(devices) = host.devices() else {
                    continue;
                };
                for device in devices {
                    drivers.push(DriverEntry {
                        host_id,
                        asio_driver: Some(device.to_string()),
                    });
                }
            } else {
                drivers.push(DriverEntry {
                    host_id,
                    asio_driver: None,
                });
            }
        }
        drivers
    }

    fn make_driver_labels(drivers: &[DriverEntry]) -> Vec<String> {
        drivers.iter().map(DriverEntry::label).collect()
    }

    fn enum_devices(host_id: cpal::HostId) -> Vec<DeviceEntry> {
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let Ok(host) = cpal::host_from_id(host_id) else {
            return entries;
        };
        let mut add = |device: cpal::Device| {
            let device_name = device.to_string();
            if seen.insert(device_name.clone()) {
                entries.push(DeviceEntry { device_name, device });
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
        entries
    }

    fn make_device_labels(entries: &[DeviceEntry], show_channels: bool) -> Vec<String> {
        entries
            .iter()
            .map(|e| Self::device_display_label(e, show_channels))
            .collect()
    }

    /// Build the combo-box label for a device. For ASIO the device *is* the driver,
    /// so the label appends the channel counts to read like a real audio interface
    /// (e.g. "Realtek ASIO (2 in / 2 out)") and stay consistent across all drivers.
    fn device_display_label(entry: &DeviceEntry, show_channels: bool) -> String {
        if show_channels {
            let ins = entry
                .device
                .default_input_config()
                .ok()
                .map(|c| c.channels());
            let outs = entry
                .device
                .default_output_config()
                .ok()
                .map(|c| c.channels());
            if let (Some(ins), Some(outs)) = (ins, outs) {
                return format!("{} ({} in / {} out)", entry.device_name, ins, outs);
            }
        }
        entry.device_name.clone()
    }

    /// Split the selected driver's devices into input- and output-capable lists.
    fn reload_devices(&mut self) {
        self.input_devices.clear();
        self.input_labels.clear();
        self.output_devices.clear();
        self.output_labels.clear();
        let Some(driver) = self.drivers.get(self.driver_current) else {
            return;
        };
        let all = Self::enum_devices(driver.host_id);
        // For an ASIO driver entry, only that specific driver's device is relevant.
        let devices: Vec<DeviceEntry> = match &driver.asio_driver {
            Some(name) => all
                .into_iter()
                .filter(|e| e.device_name == *name)
                .collect(),
            None => all,
        };
        let is_asio = driver.asio_driver.is_some();
        for entry in devices {
            if entry.device.supports_input() {
                self.input_devices.push(entry.clone());
            }
            if entry.device.supports_output() {
                self.output_devices.push(entry);
            }
        }
        self.input_labels = Self::make_device_labels(&self.input_devices, is_asio);
        self.output_labels = Self::make_device_labels(&self.output_devices, is_asio);
        self.input_current = self
            .input_current
            .min(self.input_devices.len().saturating_sub(1));
        self.output_current = self
            .output_current
            .min(self.output_devices.len().saturating_sub(1));
    }

    // Persisted selection (host + ASIO driver + input + output names), stored next
    // to the executable.
    fn settings_path() -> std::path::PathBuf {
        std::env::current_dir()
            .unwrap_or_default()
            .join("prevocal-last-device.txt")
    }

    fn load_last_selection() -> Option<(String, String, String, String)> {
        let text = std::fs::read_to_string(Self::settings_path()).ok()?;
        let mut lines = text.lines();
        let host = lines.next()?.trim().to_string();
        let asio = lines.next()?.trim().to_string();
        let input = lines.next()?.trim().to_string();
        let output = lines.next()?.trim().to_string();
        if host.is_empty() || input.is_empty() || output.is_empty() {
            return None;
        }
        Some((host, asio, input, output))
    }

    fn save_last_selection(&self) {
        let Some(driver) = self.drivers.get(self.driver_current) else {
            return;
        };
        let input = self
            .input_devices
            .get(self.input_current)
            .map(|d| d.device_name.as_str())
            .unwrap_or_default();
        let output = self
            .output_devices
            .get(self.output_current)
            .map(|d| d.device_name.as_str())
            .unwrap_or_default();
        let _ = std::fs::write(
            Self::settings_path(),
            format!(
                "{}\n{}\n{}\n{}\n",
                driver.host_id.name(),
                driver.asio_driver.as_deref().unwrap_or_default(),
                input,
                output
            ),
        );
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
        let driver = self
            .drivers
            .get(self.driver_current)
            .cloned()
            .ok_or_else(|| "No audio driver selected.".to_string())?;
        let host_id = driver.host_id;
        let host = cpal::host_from_id(host_id)
            .map_err(|e| format!("Could not load host '{}': {e}", host_id.name()))?;

        // The GUI picks the input and output device independently. Fall back to the
        // host defaults only for non-ASIO hosts: for ASIO, `default_input_device` is
        // just the first driver in the registry, which is not what the user selected.
        let input_device = self
            .input_devices
            .get(self.input_current)
            .map(|e| e.device.clone())
            .or_else(|| {
                (driver.asio_driver.is_none()).then(|| host.default_input_device()).flatten()
            })
            .ok_or_else(|| {
                format!("No input device available for host '{}'.", host_id.name())
            })?;
        let output_device = self
            .output_devices
            .get(self.output_current)
            .map(|e| e.device.clone())
            .or_else(|| {
                (driver.asio_driver.is_none()).then(|| host.default_output_device()).flatten()
            })
            .ok_or_else(|| {
                format!("No output device available for host '{}'.", host_id.name())
            })?;
        let input_name = input_device.to_string();
        let output_name = output_device.to_string();

        let output_default = output_device
            .default_output_config()
            .map_err(|e| format!("Could not query output config for '{output_device}': {e}"))?;
        let sample_rate = output_default.sample_rate();
        let output_channels = output_default.channels() as usize;
        let output_sample_format = output_default.sample_format();
        let output_config = output_default.config();
        self.sample_rate.store(sample_rate, Ordering::Relaxed);

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
            "RUNNING — in: {input_name} → out: {output_name} ({} Hz, {} in / {} out ch)",
            sample_rate, input_channels, output_channels
        ))
    }

    fn select_driver(&mut self, idx: usize) {
        if idx >= self.drivers.len() || idx == self.driver_current {
            return;
        }
        self.driver_current = idx;
        self.input_current = 0;
        self.output_current = 0;
        // Drop the current streams first: while an ASIO driver is held by an active
        // stream, cpal's ASIO enumeration stops at the first driver that is already
        // loaded (DriverAlreadyExists), hiding every other driver's devices.
        self.stop();
        self.reload_devices();
        if let Err(e) = self.start() {
            tracing::error!("Audio restart failed: {e}");
        }
    }

    fn select_input(&mut self, idx: usize) {
        if idx >= self.input_devices.len() || idx == self.input_current {
            return;
        }
        self.input_current = idx;
        if let Err(e) = self.start() {
            tracing::error!("Audio restart failed: {e}");
        }
    }

    fn select_output(&mut self, idx: usize) {
        if idx >= self.output_devices.len() || idx == self.output_current {
            return;
        }
        self.output_current = idx;
        if let Err(e) = self.start() {
            tracing::error!("Audio restart failed: {e}");
        }
    }

    fn refresh(&mut self) {
        let prev_driver = self.drivers.get(self.driver_current).cloned();
        let prev_input = self
            .input_devices
            .get(self.input_current)
            .map(|d| d.device_name.clone());
        let prev_output = self
            .output_devices
            .get(self.output_current)
            .map(|d| d.device_name.clone());
        self.stop();
        self.drivers = Self::enum_drivers();
        self.driver_labels = Self::make_driver_labels(&self.drivers);
        self.driver_current = prev_driver
            .as_ref()
            .and_then(|d| self.drivers.iter().position(|x| x.label() == d.label()))
            .unwrap_or(0);
        self.reload_devices();
        if let Some(name) = prev_input {
            self.input_current = self
                .input_devices
                .iter()
                .position(|d| d.device_name == name)
                .unwrap_or(0);
        }
        if let Some(name) = prev_output {
            self.output_current = self
                .output_devices
                .iter()
                .position(|d| d.device_name == name)
                .unwrap_or(0);
        }
        if self.input_devices.is_empty() || self.output_devices.is_empty() {
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

    fn driver_index(&self) -> i32 {
        self.driver_current as i32
    }

    fn input_index(&self) -> i32 {
        self.input_current as i32
    }

    fn output_index(&self) -> i32 {
        self.output_current as i32
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
    // Push the current driver/device selection and the engine status to the UI.
    fn sync_ui(ui: &PreVocalUI, mgr: &AudioManager) {
        ui.set_available_drivers(string_model(&mgr.driver_labels));
        ui.set_driver_index(mgr.driver_index());
        ui.set_available_inputs(string_model(&mgr.input_labels));
        ui.set_input_index(mgr.input_index());
        ui.set_available_outputs(string_model(&mgr.output_labels));
        ui.set_output_index(mgr.output_index());
        ui.set_audio_status(mgr.status().into());
        ui.set_audio_running(mgr.is_running());
    }

    ui.set_drive(bridge.drive_value());
    ui.set_hpf(bridge.hpf_value());
    ui.set_lpf(bridge.lpf_value());
    ui.set_air(bridge.air_value());
    ui.set_tube_character(bridge.tube_character_value());
    ui.set_tube_sag(bridge.tube_sag_value());
    ui.set_comp_thresh(bridge.comp_thresh_value());
    ui.set_comp_ratio(bridge.comp_ratio_value());
    ui.set_comp_attack(bridge.comp_attack_value());
    ui.set_comp_release(bridge.comp_release_value());
    ui.set_comp_makeup(bridge.comp_makeup_value());
    ui.set_delay_time(bridge.delay_time_value());
    ui.set_delay_feedback(bridge.delay_feedback_value());
    ui.set_delay_mix(bridge.delay_mix_value());
    ui.set_reverb_size(bridge.reverb_size_value());
    ui.set_reverb_damping(bridge.reverb_damping_value());
    ui.set_reverb_mix(bridge.reverb_mix_value());
    ui.set_output_trim(bridge.output_trim_value());

    let preset_names: Vec<String> = preset_names().iter().map(|s| s.to_string()).collect();
    ui.set_preset_names(string_model(&preset_names));
    ui.invoke_sync_preset_index(0);

    {
        let mgr = manager.lock().unwrap();
        sync_ui(&ui, &mgr);
    }

    let ui_weak = ui.as_weak();

    let bridge_drive = Arc::clone(&bridge);
    ui.on_drive_changed(move |v| bridge_drive.write_drive(v));

    let bridge_hpf = Arc::clone(&bridge);
    ui.on_hpf_changed(move |v| bridge_hpf.write_hpf(v));

    let bridge_lpf = Arc::clone(&bridge);
    ui.on_lpf_changed(move |v| bridge_lpf.write_lpf(v));

    let bridge_air = Arc::clone(&bridge);
    ui.on_air_changed(move |v| bridge_air.write_air(v));

    let bridge_tube_character = Arc::clone(&bridge);
    ui.on_tube_character_changed(move |v| bridge_tube_character.write_tube_character(v));

    let bridge_tube_sag = Arc::clone(&bridge);
    ui.on_tube_sag_changed(move |v| bridge_tube_sag.write_tube_sag(v));

    let bridge_comp_thresh = Arc::clone(&bridge);
    ui.on_comp_thresh_changed(move |v| bridge_comp_thresh.write_comp_thresh(v));

    let bridge_comp_ratio = Arc::clone(&bridge);
    ui.on_comp_ratio_changed(move |v| bridge_comp_ratio.write_comp_ratio(v));

    let bridge_comp_attack = Arc::clone(&bridge);
    ui.on_comp_attack_changed(move |v| bridge_comp_attack.write_comp_attack(v));

    let bridge_comp_release = Arc::clone(&bridge);
    ui.on_comp_release_changed(move |v| bridge_comp_release.write_comp_release(v));

    let bridge_comp_makeup = Arc::clone(&bridge);
    ui.on_comp_makeup_changed(move |v| bridge_comp_makeup.write_comp_makeup(v));

    let bridge_delay_time = Arc::clone(&bridge);
    ui.on_delay_time_changed(move |v| bridge_delay_time.write_delay_time(v));

    let bridge_delay_feedback = Arc::clone(&bridge);
    ui.on_delay_feedback_changed(move |v| bridge_delay_feedback.write_delay_feedback(v));

    let bridge_delay_mix = Arc::clone(&bridge);
    ui.on_delay_mix_changed(move |v| bridge_delay_mix.write_delay_mix(v));

    let bridge_reverb_size = Arc::clone(&bridge);
    ui.on_reverb_size_changed(move |v| bridge_reverb_size.write_reverb_size(v));

    let bridge_reverb_damping = Arc::clone(&bridge);
    ui.on_reverb_damping_changed(move |v| bridge_reverb_damping.write_reverb_damping(v));

    let bridge_reverb_mix = Arc::clone(&bridge);
    ui.on_reverb_mix_changed(move |v| bridge_reverb_mix.write_reverb_mix(v));

    let bridge_trim = Arc::clone(&bridge);
    ui.on_output_trim_changed(move |v| bridge_trim.write_output_trim(v));

    // The user selected a preset (dropdown or prev/next arrows): apply it and
    // refresh the knobs/faders so the UI matches the new parameter values.
    let bridge_preset = Arc::clone(&bridge);
    let weak_preset = ui_weak.clone();
    ui.on_preset_selected(move |idx: i32| {
        let idx = idx.clamp(0, PRESETS.len() as i32 - 1) as usize;
        bridge_preset.apply_preset(&PRESETS[idx]);
        if let Some(ui) = weak_preset.upgrade() {
            ui.set_drive(bridge_preset.drive_value());
            ui.set_hpf(bridge_preset.hpf_value());
            ui.set_lpf(bridge_preset.lpf_value());
            ui.set_air(bridge_preset.air_value());
            ui.set_tube_character(bridge_preset.tube_character_value());
            ui.set_tube_sag(bridge_preset.tube_sag_value());
            ui.set_comp_thresh(bridge_preset.comp_thresh_value());
            ui.set_comp_ratio(bridge_preset.comp_ratio_value());
            ui.set_comp_attack(bridge_preset.comp_attack_value());
            ui.set_comp_release(bridge_preset.comp_release_value());
            ui.set_comp_makeup(bridge_preset.comp_makeup_value());
            ui.set_comp_bypass(PRESETS[idx].comp_bypass);
            ui.set_delay_time(bridge_preset.delay_time_value());
            ui.set_delay_feedback(bridge_preset.delay_feedback_value());
            ui.set_delay_mix(bridge_preset.delay_mix_value());
            ui.set_delay_bypass(PRESETS[idx].delay_bypass);
            ui.set_reverb_size(bridge_preset.reverb_size_value());
            ui.set_reverb_damping(bridge_preset.reverb_damping_value());
            ui.set_reverb_mix(bridge_preset.reverb_mix_value());
            ui.set_reverb_bypass(PRESETS[idx].reverb_bypass);
            ui.set_output_trim(bridge_preset.output_trim_value());
        }
    });

    // The user picked a driver: repopulate the input/output device lists.
    let mgr_driver = Arc::clone(&manager);
    let weak_driver = ui_weak.clone();
    ui.on_driver_selected(move |label: slint::SharedString| {
        let mut mgr = mgr_driver.lock().unwrap();
        if let Some(idx) = mgr.driver_labels.iter().position(|l| l == label.as_str()) {
            mgr.select_driver(idx);
        }
        if let Some(ui) = weak_driver.upgrade() {
            sync_ui(&ui, &mgr);
        }
    });

    // The user picked an input device.
    let mgr_input = Arc::clone(&manager);
    let weak_input = ui_weak.clone();
    ui.on_input_selected(move |label: slint::SharedString| {
        let mut mgr = mgr_input.lock().unwrap();
        if let Some(idx) = mgr.input_labels.iter().position(|l| l == label.as_str()) {
            mgr.select_input(idx);
        }
        if let Some(ui) = weak_input.upgrade() {
            sync_ui(&ui, &mgr);
        }
    });

    // The user picked an output device.
    let mgr_output = Arc::clone(&manager);
    let weak_output = ui_weak.clone();
    ui.on_output_selected(move |label: slint::SharedString| {
        let mut mgr = mgr_output.lock().unwrap();
        if let Some(idx) = mgr.output_labels.iter().position(|l| l == label.as_str()) {
            mgr.select_output(idx);
        }
        if let Some(ui) = weak_output.upgrade() {
            sync_ui(&ui, &mgr);
        }
    });

    // Rescan the driver/device lists (e.g. after plugging in an interface).
    let mgr_refresh = Arc::clone(&manager);
    let weak_refresh = ui_weak.clone();
    ui.on_refresh_clicked(move || {
        let mut mgr = mgr_refresh.lock().unwrap();
        mgr.refresh();
        if let Some(ui) = weak_refresh.upgrade() {
            sync_ui(&ui, &mgr);
        }
    });

    // Force a restart with the current selection (applies after errors).
    let mgr_restart = Arc::clone(&manager);
    let weak_restart = ui_weak.clone();
    ui.on_restart_clicked(move || {
        let mut mgr = mgr_restart.lock().unwrap();
        if let Err(e) = mgr.start() {
            tracing::error!("Audio restart failed: {e}");
        }
        if let Some(ui) = weak_restart.upgrade() {
            sync_ui(&ui, &mgr);
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
    let sample_rate = Arc::new(AtomicU32::new(0));
    let bridge = UiBridge::new(&params, sample_rate.clone());

    // Only FemtoVG is compiled in for now (OpenGL renderer).
    let renderers = ["femtovg"];
    let mut selected: Option<&str> = None;
    for name in renderers {
        match slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name(name.to_string())
            .select()
        {
            Ok(_) => {
                selected = Some(name);
                break;
            }
            Err(e) => {
                eprintln!("Failed to select Slint winit backend with {name} renderer: {e:?}. Trying next renderer...");
            }
        }
    }
    match selected {
        Some(name) => eprintln!("Slint winit backend selected with {name} renderer"),
        None => {
            eprintln!("All Slint winit renderers failed. Giving up.");
            std::process::exit(1);
        }
    }

    // The manager owns the cpal streams, which must stay alive while the GUI
    // runs, otherwise cpal stops the audio as soon as they are dropped.
    let manager = Arc::new(Mutex::new(AudioManager::new(params, sample_rate)));

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
