//! Slint editor embedded into the DAW's window.
//!
//! The host passes a native parent window handle through [`ParentWindowHandle`].
//! We inject it into the winit `WindowAttributes` via
//! [`slint::BackendSelector::with_winit_window_attributes_hook()`] *before* the
//! winit backend creates its window, so the Slint window becomes a `WS_CHILD`
//! confined to the DAW's view (see winit's `WindowAttributes::with_parent_window`).
//!
//! The event loop and the Slint UI run on a dedicated thread (winit is created
//! with `any_thread` support on Windows/X11). Parameter edits from the UI go
//! through [`ParamSetter`]; parameter changes coming from the host/audio thread
//! are pushed back into the UI with [`slint::invoke_from_event_loop()`].
//!
//! Note: the winit backend only supports a single event loop per process, so
//! only one editor instance can be open at a time (this matches how most hosts
//! open a single plugin editor).

use std::any::Any;
use std::num::NonZeroIsize;
use std::sync::{Arc, Mutex};

use nice_plug::context::gui::{GuiContext, ParamSetter};
use nice_plug::editor::dpi::{LogicalSize, PhysicalSize, Size};
use nice_plug::editor::{Editor, ParentWindowHandle};
use nice_plug::prelude::*;
use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
use slint::winit_030::WinitWindowAccessor;

use crate::PreVocalParams;

/// Parent HWND captured from `spawn()` and applied by the window-attributes hook.
/// Only the `Win32Hwnd` variant is supported for embedding right now; other
/// platforms fall back to a regular (top-level) editor window.
static PARENT_WINDOW: Mutex<Option<NonZeroIsize>> = Mutex::new(None);

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
                    // Force embedded child window appearance: no transparency, no decorations
                    attrs.transparent = false;
                    attrs.decorations = false;
                    attrs.resizable = false;
                    attrs.visible = false; // We'll show after parenting
                    
                    if let Some(hwnd) = *PARENT_WINDOW.lock().unwrap() {
                        tracing::info!("Applying parent window hook: HWND = {:?}", hwnd);
                        let raw = raw_window_handle::RawWindowHandle::Win32(
                            raw_window_handle::Win32WindowHandle::new(hwnd),
                        );
                        attrs = unsafe { attrs.with_parent_window(Some(raw)) };
                        // Also set as owner to prevent separate taskbar entry
                        attrs = attrs.with_owner_window(hwnd.get() as isize);
                    } else {
                        tracing::warn!("Parent window hook called but no HWND available");
                    }
                    attrs
                };
                if let Err(e) = slint::BackendSelector::new()
                    .backend_name("winit".into())
                    .renderer_name("femtovg".to_string())
                    .with_winit_window_attributes_hook(hook)
                    .select()
                {
                    tracing::error!("Failed to select Slint winit backend: {:?}", e);
                } else {
                    tracing::info!("Slint winit backend selected with femtovg renderer");
                }

                let ui = match PreVocalUI::new() {
                    Ok(ui) => ui,
                    Err(e) => {
                        tracing::error!("Could not create PreVocal editor UI: {e}");
                        return;
                    }
                };

                // Force initial window size (1000x640) in case host doesn't call set_size immediately
                ui.window().set_size(slint::PhysicalSize::new(1000, 640));

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

                let _ = ui.run();
            })
            .expect("failed to spawn the editor thread");

        Box::new(SlintEditorInstance {
            thread: Some(thread),
        })
    }

    fn size(&self) -> Size {
        Size::Logical(LogicalSize::new(1000.0, 640.0))
    }

    fn set_scale_factor(&self, _factor: f64) -> bool {
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
        let _ = physical_size;
        // Request a redraw when the host resizes the editor
        if let Some(weak) = self.active.lock().unwrap().as_ref() {
            if let Some(ui) = weak.upgrade() {
                let _ = ui.window().request_redraw();
            }
        }
        true
    }
}