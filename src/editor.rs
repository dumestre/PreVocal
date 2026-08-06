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
use std::sync::{Arc, Mutex};

use nice_plug::context::gui::{GuiContext, ParamSetter};
use nice_plug::editor::dpi::{LogicalSize, PhysicalSize, Size};
use nice_plug::editor::{Editor, ParentWindowHandle};
use nice_plug::prelude::*;

use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};

use crate::{preset_names, PreVocalParams, Preset, PRESETS};

/// Parent HWND captured from `spawn()` and applied by the window-attributes hook.
/// Only the `Win32Hwnd` variant is supported for embedding right now; other
/// platforms fall back to a regular (top-level) editor window.
static PARENT_WINDOW: Mutex<Option<NonZeroIsize>> = Mutex::new(None);

/// Preferred editor size in logical pixels (reported to the host via
/// [`Editor::size`]).
const LOGICAL_WIDTH: f32 = 1000.0;
const LOGICAL_HEIGHT: f32 = 720.0;

/// Host DPI scale factor, set through [`Editor::set_scale_factor`]. Used to
/// compute the initial physical window size before the host calls `set_size`.
static SCALE_FACTOR: Mutex<f64> = Mutex::new(1.0);

/// Most recent size requested by the host through [`Editor::set_size`].
/// `set_size` can be called before the UI thread's event loop is running, in
/// which case `slint::invoke_from_event_loop` fails and the editor thread
/// applies this value itself once the window exists.
static PENDING_HOST_SIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// Messages for the persistent editor-thread. winit 0.30 only allows ONE event
/// loop per process on Windows (the `EVENT_LOOP_CREATED` flag is never reset),
/// so the loop must live on a single thread and be re-run for every editor
/// open/close cycle (the Slint backend re-uses the loop across
/// `run_event_loop()` calls on the same thread).
enum EditorMsg {
    /// Open the editor UI (the thread parks on the channel between opens).
    Open {
        parent_hwnd: Option<NonZeroIsize>,
        context: Arc<dyn GuiContext>,
    },
    /// Close the editor (sent by `SlintEditorInstance`'s drop).
    Close,
}

/// Sender end of the persistent editor-thread's channel. `None` until the
/// thread is created on the first open (and re-created if it ever dies).
static EDITOR_TX: Mutex<Option<std::sync::mpsc::Sender<EditorMsg>>> = Mutex::new(None);

slint::include_modules!();

/// The `Editor` handed to nice-plug. It holds the shared [`PreVocalParams`] plus a
/// cell with the weak handle of the currently-open UI instance, so host parameter
/// changes can be reflected in the UI from any thread. The host's [`GuiContext`]
/// is captured when the editor is spawned, so we can ask the host to resize its
/// view to our preferred size (`request_resize`).
pub struct SlintEditor {
    params: Arc<PreVocalParams>,
    active: Arc<Mutex<Option<slint::Weak<PreVocalUI>>>>,
    context: Mutex<Option<Arc<dyn GuiContext>>>,
}

impl SlintEditor {
    pub fn new(params: Arc<PreVocalParams>) -> Self {
        Self {
            params,
            active: Arc::new(Mutex::new(None)),
            context: Mutex::new(None),
        }
    }
}

/// The handle returned from [`Editor::spawn()`]. The host drops it when the
/// editor closes; on drop we clear the active UI handle and stop the event
/// loop. The underlying thread/event loop is kept alive and parked, ready for
/// the next open (see [`editor_thread_loop`]).
pub struct SlintEditorInstance {
    active: Arc<Mutex<Option<slint::Weak<PreVocalUI>>>>,
}

impl Drop for SlintEditorInstance {
    fn drop(&mut self) {
        tracing::info!("editor: closing editor (quit event loop)");
        // Stop forwarding parameter changes to the (about to be destroyed) UI.
        *self.active.lock().unwrap() = None;
        // Tell the persistent thread the editor closed, and ask the running
        // event loop to stop. If the loop is parked (not running), the quit is
        // tagged with the current loop generation and safely ignored by the
        // next run (Slint's `CustomEvent::Exit` only honors the current one).
        if let Some(tx) = EDITOR_TX.lock().unwrap().as_ref() {
            let _ = tx.send(EditorMsg::Close);
        }
        let _ = slint::quit_event_loop();
    }
}

/// Read the current (modulated) parameter values into the UI. The UI works in
/// dB/Hz while the params store linear gain for drive and output trim.
fn apply_param_values(ui: &PreVocalUI, params: &PreVocalParams) {
    ui.set_drive(util::gain_to_db(params.drive.modulated_plain_value()));
    ui.set_hpf(params.hpf.modulated_plain_value());
    ui.set_lpf(params.lpf.modulated_plain_value());
    ui.set_air(params.air.modulated_plain_value());
    ui.set_comp_thresh(params.comp_thresh.modulated_plain_value());
    ui.set_comp_ratio(params.comp_ratio.modulated_plain_value());
    ui.set_comp_attack(params.comp_attack.modulated_plain_value());
    ui.set_comp_release(params.comp_release.modulated_plain_value());
    ui.set_comp_makeup(util::gain_to_db(params.comp_makeup.modulated_plain_value()));
    ui.set_comp_bypass(params.comp_bypass.value());
    ui.set_delay_time(params.delay_time.modulated_plain_value());
    ui.set_delay_feedback(params.delay_feedback.modulated_plain_value());
    ui.set_delay_mix(params.delay_mix.modulated_plain_value());
    ui.set_delay_bypass(params.delay_bypass.value());
    ui.set_output_trim(util::gain_to_db(params.output_trim.modulated_plain_value()));
}

/// Schedule `update` on the UI thread if an editor window is currently open.
/// Safe to call from any thread (the host's audio thread included).
fn push_to_ui(active: &Mutex<Option<slint::Weak<PreVocalUI>>>, update: impl FnOnce(&PreVocalUI) + Send + 'static) {
    let weak = match active.lock().unwrap().as_ref() {
        Some(weak) => weak.clone(),
        None => return,
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            update(&ui);
        }
    });
}

/// Resize the editor window to `size` (physical pixels) on the UI thread, so
/// the child window always fills the host's view. Safe from any thread.
fn resize_editor_window(
    active: &Mutex<Option<slint::Weak<PreVocalUI>>>,
    size: (u32, u32),
) {
    let weak = match active.lock().unwrap().as_ref() {
        Some(weak) => weak.clone(),
        None => {
            tracing::debug!("editor: resize to {}x{} deferred (no UI instance)", size.0, size.1);
            return;
        }
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            tracing::info!("editor: resizing window to {}x{}", size.0, size.1);
            ui.window().set_size(slint::PhysicalSize::new(size.0, size.1));
        }
    });
}

/// The preferred size at the current host DPI scale factor (physical pixels).
fn preferred_physical_size() -> slint::PhysicalSize {
    let sf = *SCALE_FACTOR.lock().unwrap() as f32;
    slint::PhysicalSize::new(
        (LOGICAL_WIDTH * sf).round() as u32,
        (LOGICAL_HEIGHT * sf).round() as u32,
    )
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

/// Keep the embedded (software-rendered) window repainting robustly inside the
/// DAW's view. The software renderer uses a partial-repaint cache: when the OS
/// discards the window's content (minimize, cover by another app, host hiding
/// the view) the cache still believes nothing changed, so nothing is presented
/// again and the view stays black. Also, without a continuous repaint the child
/// window leaves ghost trails while it is dragged around in the host. This
/// registers a winit event filter that:
///
/// - re-arms a redraw on every `RedrawRequested`, giving a continuous repaint
///   loop (auto-stalled while the window is hidden, because Windows only
///   delivers `WM_PAINT` to visible windows),
/// - on `Occluded(false)`/`Focused(true)` forces a full repaint by bouncing the
///   window size 1px, which invalidates the partial-repaint cache so the next
///   frame is re-rendered completely.
///
/// Every window event is logged to `C:\temp\prevocal_plugin.log` (TRACE level)
/// so the repaint/black-screen behaviour can be diagnosed from the host.
fn install_redraw_event_filter(window: &slint::Window) {
    let mut redraws_since_report = 0u64;
    let mut last_report = std::time::Instant::now();
    tracing::info!("editor: installing redraw event filter");
    window.on_winit_window_event(move |window, event| {
        match event {
            WindowEvent::RedrawRequested => {
                redraws_since_report += 1;
                let now = std::time::Instant::now();
                if now.duration_since(last_report) >= std::time::Duration::from_secs(2) {
                    tracing::debug!(
                        "editor: redraws in last 2s = {} (continuous repaint loop alive)",
                        redraws_since_report
                    );
                    redraws_since_report = 0;
                    last_report = now;
                }
                window.request_redraw();
            }
            WindowEvent::Occluded(occluded) => {
                tracing::info!("editor: window event Occluded({})", occluded);
                if !*occluded {
                    let size = window.size();
                    if size.width > 0 && size.height > 0 {
                        tracing::info!(
                            "editor: forcing full repaint (occluded=false) at {}x{}",
                            size.width,
                            size.height
                        );
                        window.set_size(slint::PhysicalSize::new(size.width, size.height + 1));
                        window.set_size(size);
                    }
                    window.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                tracing::info!("editor: window event Focused({})", focused);
                if *focused {
                    let size = window.size();
                    if size.width > 0 && size.height > 0 {
                        tracing::info!(
                            "editor: forcing full repaint (focused) at {}x{}",
                            size.width,
                            size.height
                        );
                        window.set_size(slint::PhysicalSize::new(size.width, size.height + 1));
                        window.set_size(size);
                    }
                    window.request_redraw();
                }
            }
            WindowEvent::Resized(size) => {
                tracing::info!("editor: window event Resized({}x{})", size.width, size.height);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                tracing::info!("editor: window event ScaleFactorChanged({})", scale_factor);
            }
            WindowEvent::Moved(pos) => {
                tracing::debug!("editor: window event Moved({:?})", pos);
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

    setter.begin_set_parameter(&params.output_trim);
    setter.set_parameter(&params.output_trim, trim);
    setter.end_set_parameter(&params.output_trim);
}

/// Create a new editor window (parented to the host's view) and wire up the
/// UI<->parameter glue. Runs on the persistent editor thread, so the winit
/// backend's platform state is reused instead of re-initialized.
fn create_editor_ui(
    params: &Arc<PreVocalParams>,
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

    // Size the child window to the host's view. If the host already
    // called `set_size` before the event loop was running, apply that
    // size now; otherwise fall back to the logical preferred size at
    // the host's DPI scale factor.
    let initial = PENDING_HOST_SIZE.lock().unwrap().take().map_or_else(
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

    // Ask the host to resize its view to our preferred size. The wrapper
    // posts this to the host's GUI thread, so it's safe from here.
    let resize_ok = context.request_resize();
    tracing::info!("editor: request_resize() -> {}", resize_ok);

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

    *active.lock().unwrap() = Some(ui.as_weak());
    Some(ui)
}

/// Runs the single, process-wide winit event loop for this plugin. Called once
/// per process on a dedicated thread; the Slint backend keeps the event loop
/// alive between `run_event_loop()` calls on the same thread, so this parks on
/// the channel between opens and re-runs the loop for each open/close cycle.
fn editor_thread_loop(
    rx: std::sync::mpsc::Receiver<EditorMsg>,
    params: Arc<PreVocalParams>,
    active: Arc<Mutex<Option<slint::Weak<PreVocalUI>>>>,
) {
    // CRITICAL: Select the backend once, before any Slint UI code runs on this
    // thread. A second `select()` (e.g. from another thread on the next open)
    // fails: winit refuses a second event loop per process ("EventLoop can't be
    // recreated") and Slint's platform is bound to the creating thread.
    let hook = |mut attrs: slint::winit_030::winit::window::WindowAttributes| {
        // The host draws the frame around the plugin view; a child window
        // must not bring its own decorations.
        attrs.decorations = false;
        attrs.resizable = false;
        if let Some(hwnd) = *PARENT_WINDOW.lock().unwrap() {
            tracing::info!("Applying parent window hook: HWND = {:?}", hwnd);
            let raw = raw_window_handle::RawWindowHandle::Win32(
                raw_window_handle::Win32WindowHandle::new(hwnd),
            );
            // Creating the window as a WS_CHILD of the host's view up front
            // (instead of SetParent'ing it later) is what keeps the editor
            // properly embedded.
            attrs = unsafe { attrs.with_parent_window(Some(raw)) };
            // Size the child window to the host's current view rather than
            // our preferred size, so the UI fills the host's view even when
            // the host never calls `set_size`/`onSize`. The host can still
            // override this later through `set_size`.
            if let Some(size) = host_view_size(hwnd) {
                tracing::info!("Sizing child window to host view: {:?}", size);
                attrs.inner_size = Some(nice_plug::editor::dpi::Size::Physical(size));
            }
        } else {
            tracing::warn!("Parent window hook called but no HWND available");
        }
        // The Slint winit backend starts every window as hidden
        // (`visible: false`). `ui.run()` would show it, but the plugin
        // editor uses `run_event_loop()`, so the child window would
        // never appear: the host would only show an empty frame.
        attrs.visible = true;
        // Opaque UI (background #121214): skip winit's DWM blur-behind
        // path, which fails with E_INVALIDARG on WS_CHILD windows and
        // can leave the plugin view blank.
        attrs.transparent = false;
        attrs
    };
    if let Err(e) = slint::BackendSelector::new()
        .backend_name("winit".into())
        // Software renderer (GDI/softbuffer) instead of femtovg/OpenGL:
        // the glutin path (EGL/WGL) fails to present into a WS_CHILD
        // window, leaving the host's editor view blank.
        .renderer_name("sw".to_string())
        .with_winit_window_attributes_hook(hook)
        .select()
    {
        tracing::error!("Failed to select Slint winit backend: {:?}", e);
        return;
    }
    tracing::info!("Slint winit backend selected with software renderer");

    loop {
        let (parent_hwnd, context) = match rx.recv() {
            Ok(EditorMsg::Open { parent_hwnd, context }) => (parent_hwnd, context),
            Ok(EditorMsg::Close) => continue,
            Err(_) => {
                tracing::warn!("editor: editor channel closed, thread exiting");
                break;
            }
        };
        *PARENT_WINDOW.lock().unwrap() = parent_hwnd;

        let Some(ui) = create_editor_ui(&params, &active, &context) else {
            continue;
        };

        // `run_event_loop()` instead of `ui.run()`: the plugin editor must not
        // manage its own window lifetime (a `run()` on the UI assumes a
        // standalone application and fights the host's window management).
        tracing::info!("editor: running event loop");
        let _ = slint::run_event_loop();
        tracing::info!("editor: event loop finished");

        // The loop stopped (host closed the editor): destroy the window and
        // clear the active handle so parameter updates stop touching it.
        *active.lock().unwrap() = None;
        drop(ui);
        tracing::info!("editor: editor window destroyed");
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
        *PARENT_WINDOW.lock().unwrap() = parent_hwnd;
        *self.context.lock().unwrap() = Some(Arc::clone(&context));

        let params = Arc::clone(&self.params);
        let active = Arc::clone(&self.active);

        // Ensure the persistent editor thread exists, then ask it to open the
        // UI. winit only allows ONE event loop per process on Windows (the
        // `EVENT_LOOP_CREATED` flag is never reset), so the thread is created
        // once and parked between opens, re-running the loop for each open.
        let mut tx_guard = EDITOR_TX.lock().unwrap();
        let needs_thread = tx_guard.as_ref().is_none_or(|tx| {
            tx.send(EditorMsg::Open {
                parent_hwnd,
                context: Arc::clone(&context),
            })
            .is_err()
        });
        if needs_thread {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("prevocal-editor".into())
                .spawn(move || editor_thread_loop(rx, params, active))
                .expect("failed to spawn the editor thread");
            let _ = tx.send(EditorMsg::Open {
                parent_hwnd,
                context: Arc::clone(&context),
            });
            *tx_guard = Some(tx);
        }
        drop(tx_guard);

        Box::new(SlintEditorInstance {
            active: Arc::clone(&self.active),
        })
    }

    fn size(&self) -> Size {
        Size::Logical(LogicalSize::new(LOGICAL_WIDTH as f64, LOGICAL_HEIGHT as f64))
    }

    fn set_scale_factor(&self, factor: f64) -> bool {
        tracing::info!("Editor::set_scale_factor({})", factor);
        *SCALE_FACTOR.lock().unwrap() = factor;
        let size = preferred_physical_size();
        resize_editor_window(&self.active, (size.width, size.height));
        if let Some(context) = self.context.lock().unwrap().as_ref() {
            // Keep the host's view in sync with the (scaled) preferred size.
            let resize_ok = context.request_resize();
            tracing::info!("editor: request_resize() after scale change -> {}", resize_ok);
        }
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
        tracing::info!(
            "Editor::set_size({}x{})",
            physical_size.width,
            physical_size.height
        );
        // The host resized its view; keep the child window filling it. The size
        // is also stashed in case the event loop isn't running yet (the editor
        // thread applies it right after the window is created).
        *PENDING_HOST_SIZE.lock().unwrap() = Some((physical_size.width, physical_size.height));
        resize_editor_window(&self.active, (physical_size.width, physical_size.height));
        true
    }
}