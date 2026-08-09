//! Slint editor embedded into the DAW's window.
//!
//! This module is the *plugin-only* path: the host passes a native parent window
//! handle through [`ParentWindowHandle`], and we inject it into the winit
//! `WindowAttributes` via [`slint::BackendSelector::with_winit_window_attributes_hook()`]
//! *before* the winit backend creates its window, so the Slint window is born as a
//! `WS_CHILD` confined to the DAW's view (winit's
//! `WindowAttributes::with_parent_window`). We never reparent or patch styles after
//! creation (no `SetParent`/`SetWindowLongPtrW`), which is what breaks embedding.
//!
//! The event loop and the Slint UI run on a dedicated thread (winit is created
//! with `any_thread` support on Windows/X11), driven by [`slint::run_event_loop()`]
//! rather than `Window::run()` so the editor never assumes it owns an application.
//! Parameter edits from the UI go through [`ParamSetter`]; parameter changes coming
//! from the host/audio thread are pushed back into the UI with
//! [`slint::invoke_from_event_loop()`].
//!
//! Note: winit only supports a single event loop per process (on Windows the
//! `EVENT_LOOP_CREATED` flag is never reset), and Slint's platform is bound to
//! the thread that created it. The editor therefore runs on a dedicated
//! *persistent* thread that is created on the first open and parked between
//! opens, re-running the event loop for each open/close cycle. Only one editor
//! instance can be open at a time (this matches how most hosts open a single
//! plugin editor).
//!
//! The desktop standalone is a separate concern and lives in `standalone.rs`.

use std::any::Any;
use std::num::NonZeroIsize;
use std::panic;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nice_plug::context::gui::{GuiContext, ParamSetter};
use nice_plug::editor::dpi::{LogicalSize, PhysicalSize, Size};
use nice_plug::editor::{Editor, ParentWindowHandle};
use nice_plug::prelude::*;

use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};

use crate::{level_to_meter, preset_names, MeterState, PreVocalParams, Preset, PRESETS};

/// Parent HWND captured from `spawn()` and applied by the window-attributes hook.
/// Only the `Win32Hwnd` variant is supported for embedding right now; other
/// platforms fall back to a regular (top-level) editor window.
static PARENT_WINDOW: Mutex<Option<NonZeroIsize>> = Mutex::new(None);

/// Preferred editor size in logical pixels (reported to the host via
/// [`Editor::size`]).
const LOGICAL_WIDTH: f32 = 1400.0;
const LOGICAL_HEIGHT: f32 = 720.0;

/// Host DPI scale factor, set through [`Editor::set_scale_factor`]. Used to
/// compute the initial physical window size before the host calls `set_size`.
static SCALE_FACTOR: Mutex<f64> = Mutex::new(1.0);

/// Most recent size requested by the host through [`Editor::set_size`].
/// `set_size` can be called before the UI thread's event loop is running, in
/// which case `slint::invoke_from_event_loop` fails and the editor thread
/// applies this value itself once the window exists.
static PENDING_HOST_SIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// Budget of window-size self-corrections left for the current editor session.
///
/// Some hosts (FL Studio) don't honor the size we report through
/// [`Editor::size`]/[`request_resize`] and instead restore the plugin window
/// at a stored size — e.g. 1000x881 instead of our 1000x720. The child window
/// then ends up taller than the designed UI, the centered `VerticalLayout`
/// leaves a band of empty background at the top/bottom, and the content looks
/// shifted down. We correct the child window back to our preferred size, but
/// only a few times per session: a host that insists on its own size would
/// otherwise cause an endless resize fight. This is safe (no host callback is
/// involved — the correction is a plain `SetWindowPos` on our own child), the
/// budget just guards against pathological hosts.
static SIZE_CORRECTIONS_LEFT: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// How many times the child window may be forced back to the preferred size
/// per editor session (see [`SIZE_CORRECTIONS_LEFT`]).
const SIZE_CORRECTION_BUDGET: u32 = 3;

/// Messages for the persistent editor-thread. winit 0.30 only allows ONE event
/// loop per process on Windows (the `EVENT_LOOP_CREATED` flag is never reset),
/// so the loop must live on a single thread and be re-run for every editor
/// open/close cycle (the Slint backend re-uses the loop across
/// `run_event_loop()` calls on the same thread).
enum EditorMsg {
    /// Open the editor UI (the thread parks on the channel between opens). The
    /// optional ack is signalled once the window has been created, so the
    /// host's `attached()` can block until the editor window exists (hosts
    /// like Cubase expect the plugin window to be there when `attached()`
    /// returns; returning early with no window yet freezes them).
    Open {
        parent_hwnd: Option<NonZeroIsize>,
        /// Shared parameter state for THIS plugin instance. The persistent
        /// thread is process-wide and reused across instances, so all
        /// instance state (params, meters, active) must travel with each open
        /// rather than being captured at spawn.
        params: Arc<PreVocalParams>,
        meters: Arc<Mutex<MeterState>>,
        active: Arc<Mutex<Option<slint::Weak<PreVocalUI>>>>,
        context: Arc<dyn GuiContext>,
        window_created: Option<std::sync::mpsc::Sender<()>>,
    },
    /// Close the editor (sent by `SlintEditorInstance`'s drop). The sender is
    /// ack'd once the editor window has actually been destroyed, so the host's
    /// `removed()` call doesn't return while the child window still exists
    /// (hosts like FL Studio wait for the plugin window to disappear).
    Close {
        ack: std::sync::mpsc::Sender<()>,
    },
    /// Terminate the thread (sent from `ExitDll` when the host unloads the
    /// plugin DLL). The thread (and its winit event loop / windows) live
    /// *inside* the DLL, so if it keeps running after the module is unmapped
    /// it executes unmapped code → access violation → host crash.
    Shutdown,
}

/// Sender end of the persistent editor-thread's channel. `None` until the
/// thread is created on the first open (and re-created if it ever dies).
static EDITOR_TX: Mutex<Option<std::sync::mpsc::Sender<EditorMsg>>> = Mutex::new(None);

/// Join handle of the persistent editor thread. Taken (and joined) when the
/// DLL is unloaded from `shutdown_editor_thread()`.
static EDITOR_JOIN: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Join handle of the current UI-refresh thread (`spawn_ui_refresh_thread`).
/// Joined at DLL unload: that thread also lives inside the DLL's code and
/// would execute unmapped instructions if the module is unmapped while it is
/// still running. Replaced on every editor open; the previous handle's thread
/// has already exited by then (it stops as soon as its UI is gone).
static REFRESH_JOIN: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Shared liveness flag for the UI-refresh thread. `Weak::upgrade()` returns
/// None from any thread other than the one that created the Weak, so the
/// refresh thread cannot use it to detect a closed window — it polls this flag
/// instead. The editor thread clears it (and drops the Arc) right after the
/// editor window is destroyed; a new Arc is stored on every open.
static REFRESH_ALIVE: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>> = Mutex::new(None);

/// Set by `SlintEditorInstance::drop` (host/GUI thread) to signal the editor
/// thread that a close is in flight. Guards against a pending quit being
/// processed by a *later* `run_event_loop()`: if this flag is still set when
/// an `Open` arrives, the loop is not re-run (the close won the race).
static CLOSE_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The Slint/winit backend (EventLoop + platform) can be initialized **once
/// per process**. winit 0.30 refuses to create a second EventLoop ("can't be
/// recreated") — so if the editor thread ever dies and respawns we must NOT
/// call `BackendSelector::select()` again. We gate it behind this `Once`.
static SLINT_BACKEND_ONCE: std::sync::Once = std::sync::Once::new();
/// `true` if the once-init succeeded (i.e. at least one renderer backend is
/// up). If this stays `false` even after `call_once`, all UI opens fail fast.
static SLINT_BACKEND_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

slint::include_modules!();

fn lock_mutex<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("editor: mutex poisoned, recovering into_inner()");
            poisoned.into_inner()
        }
    }
}

/// Pump pending Windows messages for this thread. Our winit window and the
/// winit event loop's message-only window were created on THIS thread, so any
/// synchronous `SendMessage` a host (e.g. FL Studio) sends to them must be
/// answered by this thread. While the editor thread is parked between opens
/// there is no winit pump running, so without this a host's `SendMessage`
/// would block forever → frozen DAW. Safe: the winit WndProc handles each
/// dispatched message; unrelated messages are dispatched to their own
/// WndProcs as usual.
fn pump_pending_windows_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
}

fn try_select_renderer_backend(
    hook: impl Fn(
        slint::winit_030::winit::window::WindowAttributes,
    ) -> slint::winit_030::winit::window::WindowAttributes
        + Send
        + Sync
        + Clone
        + 'static,
) -> bool {
    // Only FemtoVG is compiled in for now (OpenGL on WS_CHILD inside DAWs).
    let renderers = ["femtovg"];
    for name in renderers {
        let hook = hook.clone();
        let result = slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name(name.to_string())
            .with_winit_window_attributes_hook(
                move |attrs: slint::winit_030::winit::window::WindowAttributes| hook(attrs),
            )
            .select();
        match result {
            Ok(_) => {
                tracing::info!("Slint winit backend selected with {name} renderer");
                return true;
            }
            Err(e) => {
                tracing::warn!("Failed to select Slint winit backend with {name} renderer: {e:?}. Trying next renderer...");
            }
        }
    }
    tracing::error!("All Slint winit renderers failed. Giving up.");
    false
}

/// Ensure the Slint/winit backend is initialized **once per process**. The
/// platform/winit EventLoop cannot be recreated, so a second
/// `BackendSelector::select()` (e.g. after the editor thread respawns) would
/// silently deadlock inside winit. We gate everything behind a static `Once`.
fn ensure_backend_initialized() -> bool {
    SLINT_BACKEND_ONCE.call_once(|| {
        tracing::info!("editor: first backend init — setting up Slint winit platform");
        let hook = |mut attrs: slint::winit_030::winit::window::WindowAttributes| {
            attrs.decorations = false;
            attrs.resizable = false;
            if let Some(hwnd) = *lock_mutex(&PARENT_WINDOW) {
                tracing::info!("Applying parent window hook: HWND = {:?}", hwnd);
                let raw = raw_window_handle::RawWindowHandle::Win32(
                    raw_window_handle::Win32WindowHandle::new(hwnd),
                );
                attrs = unsafe { attrs.with_parent_window(Some(raw)) };
                if let Some(size) = host_view_size(hwnd) {
                    let size = clamp_to_monitor(hwnd, size);
                    tracing::info!("Sizing child window to host view: {:?}", size);
                    attrs.inner_size = Some(nice_plug::editor::dpi::Size::Physical(size));
                }
            } else {
                tracing::warn!("Parent window hook called but no HWND available");
            }
            attrs.visible = true;
            attrs.transparent = false;
            attrs
        };
        let ok = try_select_renderer_backend(hook);
        SLINT_BACKEND_OK.store(ok, std::sync::atomic::Ordering::SeqCst);
    });
    SLINT_BACKEND_OK.load(std::sync::atomic::Ordering::SeqCst)
}

/// The `Editor` handed to nice-plug. It holds the shared [`PreVocalParams`] plus a
/// cell with the weak handle of the currently-open UI instance, so host parameter
/// changes can be reflected in the UI from any thread. The host's [`GuiContext`]
/// is captured when the editor is spawned, so we can ask the host to resize its
/// view to our preferred size (`request_resize`).
pub struct SlintEditor {
    params: Arc<PreVocalParams>,
    /// Shared realtime meter state (audio thread writes, UI poll loop reads).
    meters: Arc<Mutex<MeterState>>,
    active: Arc<Mutex<Option<slint::Weak<PreVocalUI>>>>,
    /// Shared with `SlintEditorInstance` so the instance's `Drop` can clear it.
    context: Arc<Mutex<Option<Arc<dyn GuiContext>>>>,
}

impl SlintEditor {
    pub fn new(params: Arc<PreVocalParams>, meters: Arc<Mutex<MeterState>>) -> Self {
        tracing::info!("editor: SlintEditor created (new plugin wrapper instance)");
        Self {
            params,
            meters,
            active: Arc::new(Mutex::new(None)),
            context: Arc::new(Mutex::new(None)),
        }
    }
}

impl Drop for SlintEditor {
    fn drop(&mut self) {
        // Diagnostic: this runs when the host destroys the plugin (delete) and
        // the wrapper tears down. If this log never appears after a delete, the
        // WrapperInner is being leaked (Arc cycle) instead of dropped.
        tracing::info!("editor: SlintEditor dropped (plugin teardown)");
    }
}

/// The handle returned from [`Editor::spawn()`]. The host drops it when the
/// editor closes; on drop we clear the active UI handle and stop the event
/// loop. The underlying thread/event loop is kept alive and parked, ready for
/// the next open (see [`editor_thread_loop`]).
pub struct SlintEditorInstance {
    active: Arc<Mutex<Option<slint::Weak<PreVocalUI>>>>,
    context: Arc<Mutex<Option<Arc<dyn GuiContext>>>>,
}

impl Drop for SlintEditorInstance {
    fn drop(&mut self) {
        tracing::info!("editor: closing editor — drop started");
        // CRITICAL: release our reference to the host's GuiContext. It holds
        // an `Arc<WrapperInner>`, and WrapperInner owns this SlintEditor, so
        // keeping it alive would create an Arc cycle: dropping the plugin
        // (host delete) would recursively drop WrapperInner → stack overflow
        // → process crash. Clearing it here (on every editor close) breaks
        // the cycle before the plugin is ever destroyed.
        *lock_mutex(&self.context) = None;
        // Snapshot the weak handle BEFORE clearing `active` so the hide/quit
        // closure (which runs on the event loop thread) can still find the UI.
        let weak = lock_mutex(&self.active).clone();
        *lock_mutex(&self.active) = None;
        CLOSE_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if let Some(tx) = lock_mutex(&EDITOR_TX).as_ref() {
            let _ = tx.send(EditorMsg::Close { ack: ack_tx });
            tracing::info!("editor: drop sent Close message");
        }
        // NEVER call `slint::quit_event_loop()` from the host's GUI thread.
        // Instead, run it *inside* the event loop thread via
        // `invoke_from_event_loop` (the documented way to quit from a
        // callback). Hide the window first so it vanishes from the screen the
        // moment the host tears the view down.
        tracing::info!("editor: drop scheduling quit on the event loop thread");
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(weak) = weak {
                if let Some(ui) = weak.upgrade() {
                    tracing::info!("editor: hiding window from event loop thread");
                    let _ = ui.window().hide();
                }
            }
            tracing::info!("editor: quit invoked on the event loop thread");
            let _ = slint::quit_event_loop();
        });
        // Block until the editor thread has destroyed the child window. The
        // host's `removed()` must not return while our window still exists:
        // hosts such as FL Studio freeze waiting for it to disappear.
        let _ = ack_rx.recv_timeout(Duration::from_secs(2));
        tracing::info!("editor: drop returned (child window destroyed)");
    }
}

/// Read the current (modulated) parameter values into the UI. The UI works in
/// dB/Hz while the params store linear gain for drive and output trim.
///
/// Each property is only written when its value actually differs from what the
/// UI already shows (with a small epsilon for the dB/gain roundtrips). This
/// keeps the refresh loop idempotent: while a knob is being dragged the param
/// already holds the dragged value, so the tick that fires from that change no
/// longer re-writes *every* knob on the same event-loop pass — the suspected
/// source of one knob visually resetting another.
fn apply_param_values(ui: &PreVocalUI, params: &PreVocalParams) {
    let drive = util::gain_to_db(params.drive.modulated_plain_value());
    if (ui.get_drive() - drive).abs() > 1e-3 {
        ui.set_drive(drive);
    }
    let hpf = params.hpf.modulated_plain_value();
    if (ui.get_hpf() - hpf).abs() > 1e-3 {
        ui.set_hpf(hpf);
    }
    let lpf = params.lpf.modulated_plain_value();
    if (ui.get_lpf() - lpf).abs() > 1e-3 {
        ui.set_lpf(lpf);
    }
    let air = params.air.modulated_plain_value();
    if (ui.get_air() - air).abs() > 1e-3 {
        ui.set_air(air);
    }
    let tube_character = params.tube_character.modulated_plain_value();
    if (ui.get_tube_character() - tube_character).abs() > 1e-4 {
        ui.set_tube_character(tube_character);
    }
    let tube_sag = params.tube_sag.modulated_plain_value();
    if (ui.get_tube_sag() - tube_sag).abs() > 1e-4 {
        ui.set_tube_sag(tube_sag);
    }
    let comp_thresh = params.comp_thresh.modulated_plain_value();
    if (ui.get_comp_thresh() - comp_thresh).abs() > 1e-3 {
        ui.set_comp_thresh(comp_thresh);
    }
    let comp_ratio = params.comp_ratio.modulated_plain_value();
    if (ui.get_comp_ratio() - comp_ratio).abs() > 1e-3 {
        ui.set_comp_ratio(comp_ratio);
    }
    let comp_attack = params.comp_attack.modulated_plain_value();
    if (ui.get_comp_attack() - comp_attack).abs() > 1e-3 {
        ui.set_comp_attack(comp_attack);
    }
    let comp_release = params.comp_release.modulated_plain_value();
    if (ui.get_comp_release() - comp_release).abs() > 1e-3 {
        ui.set_comp_release(comp_release);
    }
    let comp_makeup = util::gain_to_db(params.comp_makeup.modulated_plain_value());
    if (ui.get_comp_makeup() - comp_makeup).abs() > 1e-3 {
        ui.set_comp_makeup(comp_makeup);
    }
    let comp_bypass = params.comp_bypass.value();
    if ui.get_comp_bypass() != comp_bypass {
        ui.set_comp_bypass(comp_bypass);
    }
    let delay_time = params.delay_time.modulated_plain_value();
    if (ui.get_delay_time() - delay_time).abs() > 1e-3 {
        ui.set_delay_time(delay_time);
    }
    let delay_feedback = params.delay_feedback.modulated_plain_value();
    if (ui.get_delay_feedback() - delay_feedback).abs() > 1e-3 {
        ui.set_delay_feedback(delay_feedback);
    }
    let delay_mix = params.delay_mix.modulated_plain_value();
    if (ui.get_delay_mix() - delay_mix).abs() > 1e-3 {
        ui.set_delay_mix(delay_mix);
    }
    let delay_bypass = params.delay_bypass.value();
    if ui.get_delay_bypass() != delay_bypass {
        ui.set_delay_bypass(delay_bypass);
    }
    let reverb_size = params.reverb_size.modulated_plain_value();
    if (ui.get_reverb_size() - reverb_size).abs() > 1e-4 {
        ui.set_reverb_size(reverb_size);
    }
    let reverb_damping = params.reverb_damping.modulated_plain_value();
    if (ui.get_reverb_damping() - reverb_damping).abs() > 1e-4 {
        ui.set_reverb_damping(reverb_damping);
    }
    let reverb_mix = params.reverb_mix.modulated_plain_value();
    if (ui.get_reverb_mix() - reverb_mix).abs() > 1e-3 {
        ui.set_reverb_mix(reverb_mix);
    }
    let reverb_bypass = params.reverb_bypass.value();
    if ui.get_reverb_bypass() != reverb_bypass {
        ui.set_reverb_bypass(reverb_bypass);
    }
    let output_trim = util::gain_to_db(params.output_trim.modulated_plain_value());
    if (ui.get_output_trim() - output_trim).abs() > 1e-3 {
        ui.set_output_trim(output_trim);
    }
}

/// Schedule `update` on the UI thread if an editor window is currently open.
/// Safe to call from any thread (the host's audio thread included).
fn push_to_ui(active: &Mutex<Option<slint::Weak<PreVocalUI>>>, update: impl FnOnce(&PreVocalUI) + Send + 'static) {
    let weak = match lock_mutex(active).as_ref() {
        Some(weak) => weak.clone(),
        None => return,
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            update(&ui);
        }
    });
}

/// Cheap read of every UI-visible parameter, used to detect changes between
/// ticks of the UI refresh thread. Mirrors the mapping in [`apply_param_values`]
/// (linear drive/trim converted to dB) so the fingerprint matches exactly what
/// gets pushed to the UI.
#[derive(Clone, Copy, PartialEq)]
struct ParamFingerprint {
    drive: f32,
    hpf: f32,
    lpf: f32,
    air: f32,
    tube_character: f32,
    tube_sag: f32,
    comp_thresh: f32,
    comp_ratio: f32,
    comp_attack: f32,
    comp_release: f32,
    comp_makeup: f32,
    comp_bypass: bool,
    delay_time: f32,
    delay_feedback: f32,
    delay_mix: f32,
    delay_bypass: bool,
    reverb_size: f32,
    reverb_damping: f32,
    reverb_mix: f32,
    reverb_bypass: bool,
    output_trim: f32,
}

impl ParamFingerprint {
    fn read(params: &PreVocalParams) -> Self {
        Self {
            drive: util::gain_to_db(params.drive.modulated_plain_value()),
            hpf: params.hpf.modulated_plain_value(),
            lpf: params.lpf.modulated_plain_value(),
            air: params.air.modulated_plain_value(),
            tube_character: params.tube_character.modulated_plain_value(),
            tube_sag: params.tube_sag.modulated_plain_value(),
            comp_thresh: params.comp_thresh.modulated_plain_value(),
            comp_ratio: params.comp_ratio.modulated_plain_value(),
            comp_attack: params.comp_attack.modulated_plain_value(),
            comp_release: params.comp_release.modulated_plain_value(),
            comp_makeup: util::gain_to_db(params.comp_makeup.modulated_plain_value()),
            comp_bypass: params.comp_bypass.value(),
            delay_time: params.delay_time.modulated_plain_value(),
            delay_feedback: params.delay_feedback.modulated_plain_value(),
            delay_mix: params.delay_mix.modulated_plain_value(),
            delay_bypass: params.delay_bypass.value(),
            reverb_size: params.reverb_size.modulated_plain_value(),
            reverb_damping: params.reverb_damping.modulated_plain_value(),
            reverb_mix: params.reverb_mix.modulated_plain_value(),
            reverb_bypass: params.reverb_bypass.value(),
            output_trim: util::gain_to_db(params.output_trim.modulated_plain_value()),
        }
    }
}

/// Background thread that keeps the editor UI in sync with the realtime state.
///
/// This is the plugin's "refresh loop". Hosts don't reliably deliver every
/// `param_value_changed`/`param_values_changed` while the editor is opening —
/// nice-plug only forwards them once `is_editor_open` is set, which races the
/// host's initial state load — so the toggles/knobs can show stale values
/// (e.g. bypass showing OFF even though it's ON). Polling the params every
/// 40 ms and pushing changes closes that gap for the whole UI lifetime,
/// including preset switches and automation.
///
/// The same loop drives the IN/OUT meters, which the host never reports: the
/// DSP writes RMS/peak into the shared [`MeterState`] on every audio block and
/// this thread smooths + forwards them (same ballistics as the standalone).
///
/// The thread exits on its own once the editor window is gone. `Weak::upgrade()`
/// cannot be used as the liveness probe here (it returns None off-thread), so
/// it polls `alive`, which the editor thread clears after the window is
/// destroyed.
fn spawn_ui_refresh_thread(
    weak: slint::Weak<PreVocalUI>,
    params: Arc<PreVocalParams>,
    meters: Arc<Mutex<MeterState>>,
    alive: Arc<std::sync::atomic::AtomicBool>,
) {
    let handle = std::thread::Builder::new()
        .name("prevocal-ui-refresh".into())
        .spawn(move || {
            tracing::info!("ui refresh thread started (params + meters)");
            let mut last = ParamFingerprint::read(&params);
            let mut smooth_in = 0.0f32;
            let mut smooth_out = 0.0f32;
            let mut peak_in = 0.0f32;
            let mut peak_out = 0.0f32;
            while alive.load(std::sync::atomic::Ordering::Relaxed) {

                // Push parameter changes (initial state, preset loads, host
                // automation) as soon as they diverge from what the UI shows.
                let fp = ParamFingerprint::read(&params);
                if fp != last {
                    last = fp;
                    let params = Arc::clone(&params);
                    let _ = weak.upgrade_in_event_loop(move |ui| apply_param_values(&ui, &params));
                }

                // Meter levels with a little ballistics smoothing (same
                // coefficients as the standalone's poll loop).
                let m = match meters.lock() {
                    Ok(m) => m,
                    Err(poisoned) => poisoned.into_inner(),
                };
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

                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_input_level(smooth_in));
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_output_level(smooth_out));
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_input_peak(peak_in));
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_output_peak(peak_out));

                std::thread::sleep(Duration::from_millis(40));
            }
        })
        .expect("failed to spawn the UI refresh thread");
    // Track it so DLL unload can join it before the module is unmapped. The
    // previous session's thread (if any) is already gone — it exits when its
    // editor window is destroyed.
    *lock_mutex(&REFRESH_JOIN) = Some(handle);
}

/// The preferred size at the current host DPI scale factor (physical pixels),
/// clamped to the monitor's work area so the plugin never exceeds the screen.
fn preferred_physical_size() -> slint::PhysicalSize {
    let sf = *lock_mutex(&SCALE_FACTOR) as f32;
    let mut size = slint::PhysicalSize::new(
        (LOGICAL_WIDTH * sf).round() as u32,
        (LOGICAL_HEIGHT * sf).round() as u32,
    );
    if let Some(hwnd) = *lock_mutex(&PARENT_WINDOW) {
        if let Some(mon) = monitor_work_area(hwnd) {
            size.width = size.width.min(mon.width);
            size.height = size.height.min(mon.height);
        }
    }
    size
}

/// Client size (physical pixels) of the host's plugin view. The child window is
/// born at this size instead of our preferred size, so the UI fills the host's
/// view even when the host never calls `set_size`/`onSize`.
fn host_view_size(hwnd: NonZeroIsize) -> Option<slint::winit_030::winit::dpi::PhysicalSize<u32>> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;

    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `hwnd` is the host's view window handle handed to us by the DAW.
    if unsafe { GetClientRect(hwnd.get() as _, &mut rect) } == 0 {
        return None;
    }
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    (width > 0 && height > 0)
        .then_some(slint::winit_030::winit::dpi::PhysicalSize::new(width, height))
}

/// Work area (physical pixels) of the monitor nearest to `hwnd` (the screen
/// minus the taskbar). The plugin window must never be larger than the screen:
/// hosts often size the plugin view beyond the display (some add chrome on top
/// of the requested size), which pushes the child window off-screen and leaves
/// ghost trails while dragging.
fn monitor_work_area(hwnd: NonZeroIsize) -> Option<slint::winit_030::winit::dpi::PhysicalSize<u32>> {
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let monitor = unsafe { MonitorFromWindow(hwnd.get() as _, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY: `info` is a valid MONITORINFO of the right size, `monitor` is a
    // live HMONITOR from MonitorFromWindow above.
    if unsafe { GetMonitorInfoW(monitor as HMONITOR, &mut info) } == 0 {
        return None;
    }
    let rect = info.rcWork;
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    (width > 0 && height > 0)
        .then_some(slint::winit_030::winit::dpi::PhysicalSize::new(width, height))
}

/// Clamp a physical size so it never exceeds the monitor work area.
fn clamp_to_monitor(
    hwnd: NonZeroIsize,
    size: slint::winit_030::winit::dpi::PhysicalSize<u32>,
) -> slint::winit_030::winit::dpi::PhysicalSize<u32> {
    match monitor_work_area(hwnd) {
        Some(mon) => {
            slint::winit_030::winit::dpi::PhysicalSize::new(
                size.width.min(mon.width),
                size.height.min(mon.height),
            )
        }
        None => size,
    }
}

/// Keep the embedded window repainting robustly inside the DAW's view.
///
/// CRITICAL SAFETY FIXES (applied after the Cubase freeze reports):
///
/// 1. The old `window.set_size(h+1); window.set_size(h);` "size-bounce" hack
///    is **GONE**. It fired `Resized` events which made the host call
///    `Editor::set_size()` which bounced back as `window.set_size()` —
///    infinite event loop → entire DAW frozen solid.
///
/// 2. All redraws happen directly from the event-filter callback. The
///    callback runs **on the Slint/winit event-loop thread already**, so we
///    never need `invoke_from_event_loop` from inside it (and `slint::Window`
///    is not `Send` anyway — trying to move it across threads was what
///    triggered the `Cell<…> cannot be shared` compile error).
///
/// 3. Cooldowns / debouncing prevent event-storms from the host:
///    - `Occluded(false)` and `Focused(true)` are rate-limited to one redraw
///      every 500 ms, so rapid alt-tabs don't hammer the renderer.
///    - `Moved` is throttled to ~30 FPS, so dragging the plugin view inside
///      the host doesn't cause a cascade of unnecessary frames.
fn install_redraw_event_filter(window: &slint::Window) {
    tracing::info!("editor: installing redraw event filter");
    let last_moved = Arc::new(Mutex::new(Instant::now()));
    let last_restore = Arc::new(Mutex::new(Instant::now()));
    let moved_debounce = Duration::from_millis(33);
    let restore_cooldown = Duration::from_millis(500);
    // The size self-correction only applies shortly after the window is born:
    // that's when hosts restore a stored size (e.g. FL's 1000x881). A later
    // maximize/resize by the user is left alone (the correction must not fight
    // deliberate host layout).
    let window_born = Instant::now();
    let correction_window = Duration::from_secs(5);

    window.on_winit_window_event(move |window, event| {
        match event {
            WindowEvent::Occluded(occluded) => {
                tracing::info!("editor: window event Occluded({})", occluded);
                if !*occluded {
                    let mut guard = lock_mutex(&last_restore);
                    if guard.elapsed() < restore_cooldown {
                        tracing::debug!("editor: restore redraw skipped (cooldown)");
                        return EventResult::Propagate;
                    }
                    *guard = Instant::now();
                    drop(guard);
                    tracing::info!("editor: request_redraw on restore");
                    window.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                tracing::info!("editor: window event Focused({})", focused);
                if *focused {
                    let mut guard = lock_mutex(&last_restore);
                    if guard.elapsed() < restore_cooldown {
                        tracing::debug!("editor: focused redraw skipped (cooldown)");
                        return EventResult::Propagate;
                    }
                    *guard = Instant::now();
                    drop(guard);
                    tracing::info!("editor: request_redraw on focus");
                    window.request_redraw();
                }
            }
            WindowEvent::Moved(pos) => {
                let win_pos = window.position();
                tracing::info!(
                    "editor: window event Moved event=({}, {}) win=({}, {})",
                    pos.x,
                    pos.y,
                    win_pos.x,
                    win_pos.y
                );
                let mut guard = lock_mutex(&last_moved);
                if guard.elapsed() >= moved_debounce {
                    *guard = Instant::now();
                    drop(guard);
                    window.request_redraw();
                }
            }
            WindowEvent::Resized(size) => {
                let win_pos = window.position();
                let phys = window.size();
                let scale = window.scale_factor();
                let logical_w = phys.width as f32 / scale;
                let logical_h = phys.height as f32 / scale;
                let preferred = preferred_physical_size();
                // Query the REAL client size of the child window right now.
                // The event size comes from WM_SIZE lParam, which can lag the
                // actual geometry while the host is mid-resize (maximize /
                // minimize / restore send bursts of WM_SIZE). Slint caches the
                // size from the event; if it diverges from the real client
                // size the next frame renders at the stale (smaller) size and
                // the UI appears pushed toward the top-left corner.
                let (real_w, real_h) = window
                    .with_winit_window(|ww| {
                        let s = ww.inner_size();
                        (s.width, s.height)
                    })
                    .unwrap_or((size.width, size.height));
                if real_w != size.width || real_h != size.height {
                    tracing::warn!(
                        "editor: Resized event ({}x{}) != real client size ({}x{}) — geometry lag detected",
                        size.width,
                        size.height,
                        real_w,
                        real_h,
                    );
                }
                tracing::info!(
                    "editor: window event Resized({}x{}) pos=({}, {}) slint-phys=({}x{}) real=({}x{}) logical=({}x{}) scale={}",
                    size.width,
                    size.height,
                    win_pos.x,
                    win_pos.y,
                    phys.width,
                    phys.height,
                    real_w,
                    real_h,
                    logical_w,
                    logical_h,
                    scale
                );
                // Force a frame at the freshly-reported size. This window is a
                // WS_CHILD resized by the host via SetWindowPos; redraws are
                // coalesced on Windows, so after maximize/minimize the last
                // presented frame can be one rendered at the OLD size. This
                // redraw runs after slint caches the new size (the filter runs
                // before slint's own resize handling), so it repaints at the
                // correct geometry.
                if size.width > 0 && size.height > 0 {
                    window.request_redraw();
                }

                // Self-correct the child window size: hosts like FL Studio
                // restore the plugin at a stored size (e.g. 1000x881) instead
                // of the designed 1000x720, which pushes the centered UI down
                // and leaves a band of empty background at the top. The event
                // size is the authoritative client size right after the host's
                // SetWindowPos, so it's the right signal. Ignore the bogus
                // tiny initial events (14x14 phantom WM_SIZE) — correcting on
                // those would waste the whole budget before the host even
                // settles. Only within the first seconds after the window is
                // born (see `correction_window`), so a user maximize/resize
                // later in the session is never fought.
                if window_born.elapsed() <= correction_window
                    && size.width >= 200
                    && size.height >= 200
                    && (size.width, size.height) != (preferred.width, preferred.height)
                    && SIZE_CORRECTIONS_LEFT.load(std::sync::atomic::Ordering::SeqCst) > 0
                {
                    SIZE_CORRECTIONS_LEFT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    tracing::info!(
                        "editor: correcting child window {}x{} -> preferred {}x{} ({} corrections left)",
                        size.width,
                        size.height,
                        preferred.width,
                        preferred.height,
                        SIZE_CORRECTIONS_LEFT.load(std::sync::atomic::Ordering::SeqCst),
                    );
                    window.set_size(preferred);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                tracing::info!("editor: window event ScaleFactorChanged({})", scale_factor);
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

/// Apply every parameter of a preset to the plugin through the host's
/// `ParamSetter`, so the host records automation for the whole batch.
fn apply_preset(setter: &ParamSetter, params: &PreVocalParams, preset: &Preset) {
    let drive = util::db_to_gain(preset.drive_db);
    let makeup = util::db_to_gain(preset.comp_makeup_db);
    let trim = util::db_to_gain(preset.trim_db);

    setter.begin_set_parameter(&params.drive);
    setter.set_parameter(&params.drive, drive);
    setter.end_set_parameter(&params.drive);

    setter.begin_set_parameter(&params.hpf);
    setter.set_parameter(&params.hpf, preset.hpf_hz);
    setter.end_set_parameter(&params.hpf);

    setter.begin_set_parameter(&params.lpf);
    setter.set_parameter(&params.lpf, preset.lpf_hz);
    setter.end_set_parameter(&params.lpf);

    setter.begin_set_parameter(&params.air);
    setter.set_parameter(&params.air, preset.air_db);
    setter.end_set_parameter(&params.air);

    setter.begin_set_parameter(&params.tube_character);
    setter.set_parameter(&params.tube_character, preset.tube_character);
    setter.end_set_parameter(&params.tube_character);

    setter.begin_set_parameter(&params.tube_sag);
    setter.set_parameter(&params.tube_sag, preset.tube_sag);
    setter.end_set_parameter(&params.tube_sag);

    setter.begin_set_parameter(&params.comp_thresh);
    setter.set_parameter(&params.comp_thresh, preset.comp_thresh_db);
    setter.end_set_parameter(&params.comp_thresh);

    setter.begin_set_parameter(&params.comp_ratio);
    setter.set_parameter(&params.comp_ratio, preset.comp_ratio);
    setter.end_set_parameter(&params.comp_ratio);

    setter.begin_set_parameter(&params.comp_attack);
    setter.set_parameter(&params.comp_attack, preset.comp_attack_ms);
    setter.end_set_parameter(&params.comp_attack);

    setter.begin_set_parameter(&params.comp_release);
    setter.set_parameter(&params.comp_release, preset.comp_release_ms);
    setter.end_set_parameter(&params.comp_release);

    setter.begin_set_parameter(&params.comp_makeup);
    setter.set_parameter(&params.comp_makeup, makeup);
    setter.end_set_parameter(&params.comp_makeup);

    setter.begin_set_parameter(&params.comp_bypass);
    setter.set_parameter(&params.comp_bypass, preset.comp_bypass);
    setter.end_set_parameter(&params.comp_bypass);

    setter.begin_set_parameter(&params.delay_time);
    setter.set_parameter(&params.delay_time, preset.delay_time_ms);
    setter.end_set_parameter(&params.delay_time);

    setter.begin_set_parameter(&params.delay_feedback);
    setter.set_parameter(&params.delay_feedback, preset.delay_feedback_pct);
    setter.end_set_parameter(&params.delay_feedback);

    setter.begin_set_parameter(&params.delay_mix);
    setter.set_parameter(&params.delay_mix, preset.delay_mix_pct);
    setter.end_set_parameter(&params.delay_mix);

    setter.begin_set_parameter(&params.delay_bypass);
    setter.set_parameter(&params.delay_bypass, preset.delay_bypass);
    setter.end_set_parameter(&params.delay_bypass);

    setter.begin_set_parameter(&params.reverb_size);
    setter.set_parameter(&params.reverb_size, preset.reverb_size);
    setter.end_set_parameter(&params.reverb_size);

    setter.begin_set_parameter(&params.reverb_damping);
    setter.set_parameter(&params.reverb_damping, preset.reverb_damping);
    setter.end_set_parameter(&params.reverb_damping);

    setter.begin_set_parameter(&params.reverb_mix);
    setter.set_parameter(&params.reverb_mix, preset.reverb_mix_pct);
    setter.end_set_parameter(&params.reverb_mix);

    setter.begin_set_parameter(&params.reverb_bypass);
    setter.set_parameter(&params.reverb_bypass, preset.reverb_bypass);
    setter.end_set_parameter(&params.reverb_bypass);

    setter.begin_set_parameter(&params.output_trim);
    setter.set_parameter(&params.output_trim, trim);
    setter.end_set_parameter(&params.output_trim);
}

/// Create a new editor window (parented to the host's view) and wire up the
/// UI<->parameter glue. Runs on the persistent editor thread, so the winit
/// backend's platform state is reused instead of re-initialized.
fn create_editor_ui(
    params: &Arc<PreVocalParams>,
    meters: &Arc<Mutex<MeterState>>,
    active: &Arc<Mutex<Option<slint::Weak<PreVocalUI>>>>,
    context: &Arc<dyn GuiContext>,
) -> Option<PreVocalUI> {
    let ui = match PreVocalUI::new() {
        Ok(ui) => ui,
        Err(e) => {
            tracing::error!("Could not create PreVocal editor UI: {e}");
            return None;
        }
    };

    // The DAW hosts the plugin view, so the editor must be frameless.
    // Slint overrides the window-attributes hook's `decorations` from
    // the `.slint` `no-frame` binding, so this is what actually strips
    // the title bar and the close/minimize/maximize buttons.
    ui.set_plugin_mode(true);

    // Allow self-correcting the child window size if the host restores it at
    // a stored size that differs from our preferred 1000x720 (see
    // `SIZE_CORRECTIONS_LEFT`).
    SIZE_CORRECTIONS_LEFT.store(SIZE_CORRECTION_BUDGET, std::sync::atomic::Ordering::SeqCst);

    // Size the child window to the host's view. If the host already
    // called `set_size` before the event loop was running, apply that
    // size now; otherwise fall back to the logical preferred size at
    // the host's DPI scale factor.
    // NOTE: capture "host sized us" BEFORE the take() below — after the
    // take the slot is empty and the request_resize check would wrongly
    // fire (Cubase beeps on a resizeView right after opening).
    let host_sized = lock_mutex(&PENDING_HOST_SIZE).is_some();
    let initial = lock_mutex(&PENDING_HOST_SIZE).take().map_or_else(
        || {
            let s = preferred_physical_size();
            (s.width, s.height)
        },
        |s| s,
    );
    ui.window().set_size(slint::PhysicalSize::new(initial.0, initial.1));

    ui.set_audio_controls_visible(false);
    let preset_names: Vec<slint::SharedString> =
        preset_names().iter().map(|s| (*s).into()).collect();
    ui.set_preset_names(slint::ModelRc::new(slint::VecModel::from(preset_names)));
    ui.invoke_sync_preset_index(0);
    apply_param_values(&ui, params);

    // Continuous repaint + forced full repaint on restore/focus (see
    // the function docs): fixes black views after minimize/restore and
    // alt-tab, and ghost trails while dragging the plugin in the host.
    install_redraw_event_filter(ui.window());

    // Render the first frame as soon as the event loop starts, instead of
    // waiting for the host's first Resized event (which can take ~500ms and
    // leaves the window black in the meantime).
    ui.window().request_redraw();

    // Ask the host to resize its view to our preferred size. The wrapper
    // posts this to the host's GUI thread, so it's safe from here.
    // NOTE: only if the host hasn't sized us yet — the Cubase beeps when a
    // resizeView is requested right after opening, and it already sizes us
    // itself via getSize/onSize.
    let needs_resize = !host_sized;
    if needs_resize {
        let resize_ok = context.request_resize();
        tracing::info!("editor: request_resize() -> {}", resize_ok);
    } else {
        tracing::info!("editor: host already sized us, skipping request_resize");
    }

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_drive_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let gain = util::db_to_gain(v.clamp(0.0, 24.0));
        setter.begin_set_parameter(&p.drive);
        setter.set_parameter(&p.drive, gain);
        setter.end_set_parameter(&p.drive);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_hpf_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let hz = v.clamp(20.0, 200.0);
        setter.begin_set_parameter(&p.hpf);
        setter.set_parameter(&p.hpf, hz);
        setter.end_set_parameter(&p.hpf);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_lpf_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let hz = v.clamp(500.0, 20_000.0);
        setter.begin_set_parameter(&p.lpf);
        setter.set_parameter(&p.lpf, hz);
        setter.end_set_parameter(&p.lpf);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_air_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let db = v.clamp(0.0, 6.0);
        setter.begin_set_parameter(&p.air);
        setter.set_parameter(&p.air, db);
        setter.end_set_parameter(&p.air);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_comp_thresh_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let db = v.clamp(-60.0, 0.0);
        setter.begin_set_parameter(&p.comp_thresh);
        setter.set_parameter(&p.comp_thresh, db);
        setter.end_set_parameter(&p.comp_thresh);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_comp_ratio_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let ratio = v.clamp(1.0, 20.0);
        setter.begin_set_parameter(&p.comp_ratio);
        setter.set_parameter(&p.comp_ratio, ratio);
        setter.end_set_parameter(&p.comp_ratio);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_comp_attack_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let ms = v.clamp(0.1, 100.0);
        setter.begin_set_parameter(&p.comp_attack);
        setter.set_parameter(&p.comp_attack, ms);
        setter.end_set_parameter(&p.comp_attack);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_comp_release_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let ms = v.clamp(10.0, 1_000.0);
        setter.begin_set_parameter(&p.comp_release);
        setter.set_parameter(&p.comp_release, ms);
        setter.end_set_parameter(&p.comp_release);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_comp_makeup_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let gain = util::db_to_gain(v.clamp(0.0, 24.0));
        setter.begin_set_parameter(&p.comp_makeup);
        setter.set_parameter(&p.comp_makeup, gain);
        setter.end_set_parameter(&p.comp_makeup);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_comp_bypass_toggled(move |v| {
        let setter = ParamSetter::new(&*ctx);
        setter.begin_set_parameter(&p.comp_bypass);
        setter.set_parameter(&p.comp_bypass, v);
        setter.end_set_parameter(&p.comp_bypass);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_delay_time_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let ms = v.clamp(1.0, 1_000.0);
        setter.begin_set_parameter(&p.delay_time);
        setter.set_parameter(&p.delay_time, ms);
        setter.end_set_parameter(&p.delay_time);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_delay_feedback_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let pct = v.clamp(0.0, 90.0);
        setter.begin_set_parameter(&p.delay_feedback);
        setter.set_parameter(&p.delay_feedback, pct);
        setter.end_set_parameter(&p.delay_feedback);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_delay_mix_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let pct = v.clamp(0.0, 100.0);
        setter.begin_set_parameter(&p.delay_mix);
        setter.set_parameter(&p.delay_mix, pct);
        setter.end_set_parameter(&p.delay_mix);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_delay_bypass_toggled(move |v| {
        let setter = ParamSetter::new(&*ctx);
        setter.begin_set_parameter(&p.delay_bypass);
        setter.set_parameter(&p.delay_bypass, v);
        setter.end_set_parameter(&p.delay_bypass);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_reverb_size_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let size = v.clamp(0.0, 1.0);
        setter.begin_set_parameter(&p.reverb_size);
        setter.set_parameter(&p.reverb_size, size);
        setter.end_set_parameter(&p.reverb_size);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_reverb_damping_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let damping = v.clamp(0.0, 1.0);
        setter.begin_set_parameter(&p.reverb_damping);
        setter.set_parameter(&p.reverb_damping, damping);
        setter.end_set_parameter(&p.reverb_damping);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_reverb_mix_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let pct = v.clamp(0.0, 100.0);
        setter.begin_set_parameter(&p.reverb_mix);
        setter.set_parameter(&p.reverb_mix, pct);
        setter.end_set_parameter(&p.reverb_mix);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_reverb_bypass_toggled(move |v| {
        let setter = ParamSetter::new(&*ctx);
        setter.begin_set_parameter(&p.reverb_bypass);
        setter.set_parameter(&p.reverb_bypass, v);
        setter.end_set_parameter(&p.reverb_bypass);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_tube_character_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        setter.begin_set_parameter(&p.tube_character);
        setter.set_parameter(&p.tube_character, v.clamp(0.0, 1.0));
        setter.end_set_parameter(&p.tube_character);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_tube_sag_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        setter.begin_set_parameter(&p.tube_sag);
        setter.set_parameter(&p.tube_sag, v.clamp(0.0, 1.0));
        setter.end_set_parameter(&p.tube_sag);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_output_trim_changed(move |v| {
        let setter = ParamSetter::new(&*ctx);
        let gain = util::db_to_gain(v.clamp(-12.0, 12.0));
        setter.begin_set_parameter(&p.output_trim);
        setter.set_parameter(&p.output_trim, gain);
        setter.end_set_parameter(&p.output_trim);
    });

    let ctx = Arc::clone(context);
    let p = Arc::clone(params);
    ui.on_preset_selected(move |idx: i32| {
        let idx = idx.clamp(0, PRESETS.len() as i32 - 1) as usize;
        let setter = ParamSetter::new(&*ctx);
        apply_preset(&setter, &p, &PRESETS[idx]);
    });

    *lock_mutex(&active) = Some(ui.as_weak());

    // Keep the UI in sync with the realtime params and meters for the whole
    // lifetime of this editor session (see `spawn_ui_refresh_thread`). The
    // alive flag tells the refresh thread when this window is gone.
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    *lock_mutex(&REFRESH_ALIVE) = Some(Arc::clone(&alive));
    spawn_ui_refresh_thread(ui.as_weak(), Arc::clone(params), Arc::clone(meters), alive);

    Some(ui)
}

/// Runs the single, process-wide winit event loop for this plugin. Called once
/// per process on a dedicated thread; the Slint backend keeps the event loop
/// alive between `run_event_loop()` calls on the same thread, so this parks on
/// the channel between opens and re-runs the loop for each open/close cycle.
/// Stop the persistent editor thread. Called from the plugin's `exit_dll()`
/// (i.e. the VST3 `ExitDll` entry point) when the host unloads the plugin DLL.
///
/// The thread, its winit event loop and its windows all live inside the DLL's
/// code. If the module is unmapped while the thread is still running, the
/// thread (and the OS, destroying the thread's windows via the winit `WndProc`)
/// will execute unmapped code → access violation → host crash. So before the
/// DLL goes away we must: quit any running event loop, tell the thread to exit,
/// and join it. This runs under the loader lock (DllMain), so it must not load
/// libraries — it doesn't.
pub fn shutdown_editor_thread() {
    tracing::info!("editor: shutdown requested (host unloading plugin DLL)");
    // Wake the winit event loop if an editor window is currently open. If the
    // loop is parked this post stays pending and dies with the thread.
    let _ = slint::quit_event_loop();
    // Ask the thread to exit (it breaks out of the park loop immediately).
    if let Some(tx) = lock_mutex(&EDITOR_TX).as_ref() {
        let _ = tx.send(EditorMsg::Shutdown);
    }
    // Wait for the thread to finish so no code from this DLL runs (and no
    // window of this DLL survives) after we return and the module is unmapped.
    // The thread exits in well under a second; if it ever wedged, joining here
    // would block the host's unload (which is at least not a crash).
    if let Some(handle) = lock_mutex(&EDITOR_JOIN).take() {
        tracing::info!("editor: joining editor thread...");
        let _ = handle.join();
        tracing::info!("editor: editor thread joined, DLL unload safe");
    }
    // The UI-refresh thread also lives inside this DLL. By now the editor
    // thread has dropped the UI, so the refresh thread's alive flag is clear and
    // it exits on its next poll (≤40 ms); joining guarantees it is gone before
    // the module is unmapped.
    if let Some(alive) = lock_mutex(&REFRESH_ALIVE).as_ref() {
        alive.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    lock_mutex(&REFRESH_ALIVE).take();
    if let Some(handle) = lock_mutex(&REFRESH_JOIN).take() {
        tracing::info!("editor: joining UI refresh thread...");
        let _ = handle.join();
        tracing::info!("editor: UI refresh thread joined, DLL unload safe");
    }
}

fn editor_thread_loop(rx: std::sync::mpsc::Receiver<EditorMsg>) {
    // CRITICAL: Select the backend ONCE per process, before any Slint UI code
    // runs on this thread. The platform/EventLoop is bound to the creating
    // thread and winit refuses a second event loop ("EventLoop can't be
    // recreated"). If this thread ever dies and respawns, a second `select()`
    // would silently deadlock the whole DAW — `ensure_backend_initialized()`
    // gates it behind a static `Once` instead.
    //
    // Only FemtoVG is compiled in for now (OpenGL on WS_CHILD via winit).
    if !ensure_backend_initialized() {
        tracing::error!("Giving up: no Slint renderer backend could start.");
        return;
    }

    let mut heartbeat: u32 = 0;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(EditorMsg::Open { parent_hwnd, params, meters, active, context, window_created }) => {
                tracing::info!("editor: received Open message");
                *lock_mutex(&PARENT_WINDOW) = parent_hwnd;

                let Some(ui) = create_editor_ui(&params, &meters, &active, &context) else {
                    // Still ack so the host's `attached()` doesn't wait out the
                    // full timeout when the window failed to be created.
                    if let Some(ack) = window_created {
                        let _ = ack.send(());
                    }
                    continue;
                };

                // The window now exists. Ack the host's `attached()` call so it
                // can proceed with a real window to parent itself to.
                if let Some(ack) = window_created {
                    tracing::info!("editor: window created — acking host attached()");
                    let _ = ack.send(());
                }

                tracing::info!(
                    "editor: window up slint-phys=({}x{}) pos=({}, {}) scale={}",
                    ui.window().size().width,
                    ui.window().size().height,
                    ui.window().position().x,
                    ui.window().position().y,
                    ui.window().scale_factor()
                );

                tracing::info!("editor: running event loop");
                let result = panic::catch_unwind(|| {
                    if !CLOSE_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        let _ = slint::run_event_loop();
                    } else {
                        tracing::warn!("editor: close raced the open — skipping event loop run");
                    }
                });
                if let Err(panic_payload) = result {
                    let msg = panic_payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| panic_payload.downcast_ref::<String>().map(|s| s.as_str()))
                        .unwrap_or("<non-string panic payload>");
                    tracing::error!("editor: event loop PANICKED: {msg}");
                    let _ = std::fs::write(
                        r"C:\temp\prevocal_panic.log",
                        format!(
                            "[{}] EDITOR EVENT LOOP PANIC: {msg}\n",
                            chrono::Local::now().format("%H:%M:%S%.3f")
                        ),
                    );
                }
                tracing::info!("editor: event loop finished");

                *lock_mutex(&active) = None;
                drop(ui);
                // Tell the UI-refresh thread its window is gone; it exits on
                // its next poll (≤40 ms) so `shutdown_editor_thread` can join it.
                if let Some(alive) = lock_mutex(&REFRESH_ALIVE).as_ref() {
                    alive.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                lock_mutex(&REFRESH_ALIVE).take();
                tracing::info!("editor: editor window destroyed");
                tracing::info!("editor: thread parked, waiting for Open/Close message");
            }
            Ok(EditorMsg::Close { ack }) => {
                tracing::info!("editor: received Close message — clearing close flag, acking");
                // A pending quit (if any) can only have been processed while
                // the loop was running; once the Close is acked the window is
                // gone and the next open must run the loop normally. Without
                // clearing this, every second open would be skipped → black UI.
                CLOSE_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
                let _ = ack.send(());
            }
            Ok(EditorMsg::Shutdown) => {
                tracing::info!("editor: received Shutdown — thread exiting (DLL unloading)");
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                heartbeat = heartbeat.wrapping_add(1);
                if heartbeat % 10 == 1 {
                    // Heartbeat (≈ every 1s): proves this thread is alive and
                    // parked while the host is frozen.
                    tracing::debug!("editor: heartbeat — thread alive, parked");
                    // Also prove the winit event loop thread is alive: the
                    // closure runs on the UI thread via the event loop proxy.
                    let _ = slint::invoke_from_event_loop(|| {
                        tracing::info!("editor: UI thread heartbeat");
                    });
                }
                // CRITICAL: answer synchronous messages from hosts (see
                // `pump_pending_windows_messages`). Without this, a host that
                // SendMessages our winit windows while we're parked deadlocks
                // the whole DAW.
                pump_pending_windows_messages();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("editor: editor channel closed, thread exiting");
                break;
            }
        }
    }
}

impl Editor for SlintEditor {
    fn spawn(&self, parent: ParentWindowHandle, context: Arc<dyn GuiContext>) -> Box<dyn Any> {
        let parent_hwnd = match parent {
            ParentWindowHandle::Win32Hwnd(hwnd) => {
                tracing::info!("Plugin editor spawn: got parent HWND = {:?}", hwnd);
                Some(hwnd)
            }
            _ => {
                tracing::warn!("Plugin editor spawn: no Win32 parent window handle provided");
                None
            }
        };
        *lock_mutex(&PARENT_WINDOW) = parent_hwnd;
        *lock_mutex(&self.context) = Some(Arc::clone(&context));

        let params = Arc::clone(&self.params);
        let meters = Arc::clone(&self.meters);
        let active = Arc::clone(&self.active);

        // Block the host's `attached()` until the editor thread has actually
        // created the window. Hosts like Cubase expect the plugin window to
        // exist when `attached()` returns; returning before the window exists
        // leaves them waiting on a window that is only born milliseconds later
        // (on our editor thread) and the host GUI freezes.
        let (created_tx, created_rx) = std::sync::mpsc::channel();
        let mut tx_guard = lock_mutex(&EDITOR_TX);
        let needs_thread = tx_guard.as_ref().is_none_or(|tx| {
            tx.send(EditorMsg::Open {
                parent_hwnd,
                params: Arc::clone(&params),
                meters: Arc::clone(&meters),
                active: Arc::clone(&active),
                context: Arc::clone(&context),
                window_created: Some(created_tx.clone()),
            })
            .is_err()
        });
        if needs_thread {
            let (tx, rx) = std::sync::mpsc::channel();
            let handle = std::thread::Builder::new()
                .name("prevocal-editor".into())
                .spawn(move || editor_thread_loop(rx))
                .expect("failed to spawn the editor thread");
            *lock_mutex(&EDITOR_JOIN) = Some(handle);
            let _ = tx.send(EditorMsg::Open {
                parent_hwnd,
                params: Arc::clone(&params),
                meters: Arc::clone(&meters),
                active: Arc::clone(&active),
                context: Arc::clone(&context),
                window_created: Some(created_tx.clone()),
            });
            *tx_guard = Some(tx);
        }
        drop(tx_guard);

        tracing::info!("editor: spawn waiting for window creation ack");
        let _ = created_rx.recv_timeout(Duration::from_secs(2));
        tracing::info!("editor: spawn window creation confirmed");

        Box::new(SlintEditorInstance {
            active: Arc::clone(&self.active),
            context: Arc::clone(&self.context),
        })
    }

    fn size(&self) -> Size {
        if let Some(hwnd) = *lock_mutex(&PARENT_WINDOW) {
            if let Some(mon) = monitor_work_area(hwnd) {
                let sf = *lock_mutex(&SCALE_FACTOR);
                let max_w = mon.width as f64 / sf;
                let max_h = mon.height as f64 / sf;
                return Size::Logical(LogicalSize::new(
                    (LOGICAL_WIDTH as f64).min(max_w),
                    (LOGICAL_HEIGHT as f64).min(max_h),
                ));
            }
        }
        Size::Logical(LogicalSize::new(LOGICAL_WIDTH as f64, LOGICAL_HEIGHT as f64))
    }

    fn set_scale_factor(&self, factor: f64) -> bool {
        tracing::info!("Editor::set_scale_factor({})", factor);
        *lock_mutex(&SCALE_FACTOR) = factor;
        // NOTE: no window resize and no host request_resize() here. We run on
        // the host GUI thread while the wrapper holds the editor mutex:
        //  - request_resize() would execute reentrantly on this same thread
        //    (main thread shortcut in nice-plug schedule_gui) and
        //    self-deadlock on that mutex (Cubase freeze);
        //  - a queued winit resize would fight the host's own WM_SIZE a few
        //    hundred ms later, shrinking the child below the host's view
        //    (clipped/misplaced UI after minimize/restore).
        // The host sizes us itself via getSize/onSize; we only track the
        // scale factor for the next create_editor_ui().
        tracing::info!("editor: set_scale_factor returning true");
        true
    }

    fn param_value_changed(&self, id: &str, _normalized: f32) {
        let params = Arc::clone(&self.params);
        let id = id.to_string();
        push_to_ui(&self.active, move |ui| match id.as_str() {
            "drive" => ui.set_drive(util::gain_to_db(params.drive.modulated_plain_value())),
            "hpf" => ui.set_hpf(params.hpf.modulated_plain_value()),
            "lpf" => ui.set_lpf(params.lpf.modulated_plain_value()),
            "air" => ui.set_air(params.air.modulated_plain_value()),
            "tube_character" => ui.set_tube_character(params.tube_character.modulated_plain_value()),
            "tube_sag" => ui.set_tube_sag(params.tube_sag.modulated_plain_value()),
            "comp_thresh" => ui.set_comp_thresh(params.comp_thresh.modulated_plain_value()),
            "comp_ratio" => ui.set_comp_ratio(params.comp_ratio.modulated_plain_value()),
            "comp_attack" => ui.set_comp_attack(params.comp_attack.modulated_plain_value()),
            "comp_release" => ui.set_comp_release(params.comp_release.modulated_plain_value()),
            "comp_makeup" => {
                ui.set_comp_makeup(util::gain_to_db(params.comp_makeup.modulated_plain_value()))
            }
            "comp_bypass" => ui.set_comp_bypass(params.comp_bypass.value()),
            "delay_time" => ui.set_delay_time(params.delay_time.modulated_plain_value()),
            "delay_feedback" => {
                ui.set_delay_feedback(params.delay_feedback.modulated_plain_value())
            }
            "delay_mix" => ui.set_delay_mix(params.delay_mix.modulated_plain_value()),
            "delay_bypass" => ui.set_delay_bypass(params.delay_bypass.value()),
            "reverb_size" => ui.set_reverb_size(params.reverb_size.modulated_plain_value()),
            "reverb_damping" => {
                ui.set_reverb_damping(params.reverb_damping.modulated_plain_value())
            }
            "reverb_mix" => ui.set_reverb_mix(params.reverb_mix.modulated_plain_value()),
            "reverb_bypass" => ui.set_reverb_bypass(params.reverb_bypass.value()),
            "output_trim" => {
                ui.set_output_trim(util::gain_to_db(params.output_trim.modulated_plain_value()))
            }
            _ => {}
        });
    }

    fn param_modulation_changed(&self, id: &str, _normalized: f32) {
        let params = Arc::clone(&self.params);
        let id = id.to_string();
        push_to_ui(&self.active, move |ui| match id.as_str() {
            "drive" => ui.set_drive(util::gain_to_db(params.drive.modulated_plain_value())),
            "hpf" => ui.set_hpf(params.hpf.modulated_plain_value()),
            "lpf" => ui.set_lpf(params.lpf.modulated_plain_value()),
            "air" => ui.set_air(params.air.modulated_plain_value()),
            "tube_character" => ui.set_tube_character(params.tube_character.modulated_plain_value()),
            "tube_sag" => ui.set_tube_sag(params.tube_sag.modulated_plain_value()),
            "comp_thresh" => ui.set_comp_thresh(params.comp_thresh.modulated_plain_value()),
            "comp_ratio" => ui.set_comp_ratio(params.comp_ratio.modulated_plain_value()),
            "comp_attack" => ui.set_comp_attack(params.comp_attack.modulated_plain_value()),
            "comp_release" => ui.set_comp_release(params.comp_release.modulated_plain_value()),
            "comp_makeup" => {
                ui.set_comp_makeup(util::gain_to_db(params.comp_makeup.modulated_plain_value()))
            }
            "comp_bypass" => ui.set_comp_bypass(params.comp_bypass.value()),
            "delay_time" => ui.set_delay_time(params.delay_time.modulated_plain_value()),
            "delay_feedback" => {
                ui.set_delay_feedback(params.delay_feedback.modulated_plain_value())
            }
            "delay_mix" => ui.set_delay_mix(params.delay_mix.modulated_plain_value()),
            "delay_bypass" => ui.set_delay_bypass(params.delay_bypass.value()),
            "reverb_size" => ui.set_reverb_size(params.reverb_size.modulated_plain_value()),
            "reverb_damping" => {
                ui.set_reverb_damping(params.reverb_damping.modulated_plain_value())
            }
            "reverb_mix" => ui.set_reverb_mix(params.reverb_mix.modulated_plain_value()),
            "reverb_bypass" => ui.set_reverb_bypass(params.reverb_bypass.value()),
            "output_trim" => {
                ui.set_output_trim(util::gain_to_db(params.output_trim.modulated_plain_value()))
            }
            _ => {}
        });
    }

    fn param_values_changed(&self) {
        let params = Arc::clone(&self.params);
        push_to_ui(&self.active, move |ui| apply_param_values(ui, &params));
    }

    fn set_size(&self, physical_size: PhysicalSize<u32>) -> bool {
        let clamped = if let Some(hwnd) = *lock_mutex(&PARENT_WINDOW) {
            clamp_to_monitor(hwnd, physical_size)
        } else {
            physical_size
        };
        tracing::info!(
            "Editor::set_size({}x{}) -> clamped {}x{}",
            physical_size.width,
            physical_size.height,
            clamped.width,
            clamped.height
        );
        *lock_mutex(&PENDING_HOST_SIZE) = Some((clamped.width, clamped.height));
        // No queued winit resize here: the host is about to apply this size
        // to the view itself (WM_SIZE), and a deferred set_size would fight
        // it moments later, leaving the child smaller than the host's view
        // (clipped/misplaced UI). PENDING_HOST_SIZE is reapplied on the next
        // create_editor_ui() when the window is (re)born.
        true
    }
}