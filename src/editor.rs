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
//! Note: the winit backend only supports a single event loop per process, so
//! only one editor instance can be open at a time (this matches how most hosts
//! open a single plugin editor).
//!
//! The desktop standalone is a separate concern and lives in `standalone.rs`.

use std::any::Any;
use std::num::NonZeroIsize;
use std::sync::{Arc, Mutex};

use nice_plug::context::gui::{GuiContext, ParamSetter};
use nice_plug::editor::dpi::{LogicalSize, PhysicalSize, Size};
use nice_plug::editor::{Editor, ParentWindowHandle};
use nice_plug::prelude::*;

use crate::PreVocalParams;

/// Parent HWND captured from `spawn()` and applied by the window-attributes hook.
/// Only the `Win32Hwnd` variant is supported for embedding right now; other
/// platforms fall back to a regular (top-level) editor window.
static PARENT_WINDOW: Mutex<Option<NonZeroIsize>> = Mutex::new(None);

/// Preferred editor size in logical pixels (reported to the host via
/// [`Editor::size`]).
const LOGICAL_WIDTH: f32 = 1000.0;
const LOGICAL_HEIGHT: f32 = 640.0;

/// Host DPI scale factor, set through [`Editor::set_scale_factor`]. Used to
/// compute the initial physical window size before the host calls `set_size`.
static SCALE_FACTOR: Mutex<f64> = Mutex::new(1.0);

/// Most recent size requested by the host through [`Editor::set_size`].
/// `set_size` can be called before the UI thread's event loop is running, in
/// which case `slint::invoke_from_event_loop` fails and the editor thread
/// applies this value itself once the window exists.
static PENDING_HOST_SIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

slint::include_modules!();

/// The `Editor` handed to nice-plug. It holds the shared [`PreVocalParams`] plus a
/// cell with the weak handle of the currently-open UI instance, so host parameter
/// changes can be reflected in the UI from any thread.
pub struct SlintEditor {
    params: Arc<PreVocalParams>,
    active: Arc<Mutex<Option<slint::Weak<PreVocalUI>>>>,
}

impl SlintEditor {
    pub fn new(params: Arc<PreVocalParams>) -> Self {
        Self {
            params,
            active: Arc::new(Mutex::new(None)),
        }
    }
}

/// The handle returned from [`Editor::spawn()`]. The host drops it when the editor
/// closes; on drop we shut down the event loop and join the UI thread.
pub struct SlintEditorInstance {
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SlintEditorInstance {
    fn drop(&mut self) {
        let _ = slint::quit_event_loop();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Read the current (modulated) parameter values into the UI. The UI works in
/// dB/Hz while the params store linear gain for drive and output trim.
fn apply_param_values(ui: &PreVocalUI, params: &PreVocalParams) {
    ui.set_drive(util::gain_to_db(params.drive.modulated_plain_value()));
    ui.set_hpf(params.hpf.modulated_plain_value());
    ui.set_air(params.air.modulated_plain_value());
    ui.set_phase_flip(params.phase_flip.modulated_plain_value());
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
        None => return,
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
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

        let params = Arc::clone(&self.params);
        let active = Arc::clone(&self.active);
        let context = Arc::clone(&context);

        let thread = std::thread::Builder::new()
            .name("prevocal-editor".into())
            .spawn(move || {
                // CRITICAL: Select backend FIRST, before any Slint UI code runs in this thread
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
                } else {
                    tracing::info!("Slint winit backend selected with software renderer");
                }

                let ui = match PreVocalUI::new() {
                    Ok(ui) => ui,
                    Err(e) => {
                        tracing::error!("Could not create PreVocal editor UI: {e}");
                        return;
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
                apply_param_values(&ui, &params);

                let ctx = Arc::clone(&context);
                let p = Arc::clone(&params);
                ui.on_drive_changed(move |v| {
                    let setter = ParamSetter::new(&*ctx);
                    let gain = util::db_to_gain(v.clamp(0.0, 24.0));
                    setter.begin_set_parameter(&p.drive);
                    setter.set_parameter(&p.drive, gain);
                    setter.end_set_parameter(&p.drive);
                });

                let ctx = Arc::clone(&context);
                let p = Arc::clone(&params);
                ui.on_hpf_changed(move |v| {
                    let setter = ParamSetter::new(&*ctx);
                    let hz = v.clamp(20.0, 200.0);
                    setter.begin_set_parameter(&p.hpf);
                    setter.set_parameter(&p.hpf, hz);
                    setter.end_set_parameter(&p.hpf);
                });

                let ctx = Arc::clone(&context);
                let p = Arc::clone(&params);
                ui.on_air_changed(move |v| {
                    let setter = ParamSetter::new(&*ctx);
                    let db = v.clamp(0.0, 6.0);
                    setter.begin_set_parameter(&p.air);
                    setter.set_parameter(&p.air, db);
                    setter.end_set_parameter(&p.air);
                });

                let ctx = Arc::clone(&context);
                let p = Arc::clone(&params);
                ui.on_phase_flip_changed(move |v| {
                    let setter = ParamSetter::new(&*ctx);
                    setter.begin_set_parameter(&p.phase_flip);
                    setter.set_parameter(&p.phase_flip, v);
                    setter.end_set_parameter(&p.phase_flip);
                });

                let ctx = Arc::clone(&context);
                let p = Arc::clone(&params);
                ui.on_output_trim_changed(move |v| {
                    let setter = ParamSetter::new(&*ctx);
                    let gain = util::db_to_gain(v.clamp(-12.0, 12.0));
                    setter.begin_set_parameter(&p.output_trim);
                    setter.set_parameter(&p.output_trim, gain);
                    setter.end_set_parameter(&p.output_trim);
                });

                *active.lock().unwrap() = Some(ui.as_weak());

                // `run_event_loop()` instead of `ui.run()`: the plugin editor must not
                // manage its own window lifetime (a `run()` on the UI assumes a
                // standalone application and fights the host's window management).
                let _ = slint::run_event_loop();
            })
            .expect("failed to spawn the editor thread");

        Box::new(SlintEditorInstance {
            thread: Some(thread),
        })
    }

    fn size(&self) -> Size {
        Size::Logical(LogicalSize::new(LOGICAL_WIDTH as f64, LOGICAL_HEIGHT as f64))
    }

    fn set_scale_factor(&self, factor: f64) -> bool {
        *SCALE_FACTOR.lock().unwrap() = factor;
        let size = preferred_physical_size();
        resize_editor_window(&self.active, (size.width, size.height));
        true
    }

    fn param_value_changed(&self, id: &str, _normalized: f32) {
        let params = Arc::clone(&self.params);
        let id = id.to_string();
        push_to_ui(&self.active, move |ui| match id.as_str() {
            "drive" => ui.set_drive(util::gain_to_db(params.drive.modulated_plain_value())),
            "hpf" => ui.set_hpf(params.hpf.modulated_plain_value()),
            "air" => ui.set_air(params.air.modulated_plain_value()),
            "phase_flip" => ui.set_phase_flip(params.phase_flip.modulated_plain_value()),
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
            "air" => ui.set_air(params.air.modulated_plain_value()),
            "phase_flip" => ui.set_phase_flip(params.phase_flip.modulated_plain_value()),
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
        // The host resized its view; keep the child window filling it. The size
        // is also stashed in case the event loop isn't running yet (the editor
        // thread applies it right after the window is created).
        *PENDING_HOST_SIZE.lock().unwrap() = Some((physical_size.width, physical_size.height));
        resize_editor_window(&self.active, (physical_size.width, physical_size.height));
        true
    }
}