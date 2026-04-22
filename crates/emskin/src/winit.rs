use std::time::Duration;

use smithay::{
    backend::{
        input::KeyState,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                surface::render_elements_from_surface_tree, texture::TextureRenderElement, Id, Kind,
            },
            gles::{GlesRenderer, GlesTexture},
            utils::{import_surface_tree, RendererSurfaceStateUserData},
            Renderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::{
        keyboard::FilterResult,
        pointer::{CursorImageStatus, CursorImageSurfaceData},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Logical, Physical, Rectangle, Size, Transform, SERIAL_COUNTER},
    wayland::compositor::with_states,
};

pub use effect_core::CustomElement;

use crate::EmskinState;

const REFRESH_RATE: i32 = 60_000;

fn make_mode(size: Size<i32, Physical>) -> Mode {
    Mode {
        size,
        refresh: REFRESH_RATE,
    }
}

fn apply_pending_state(state: &mut EmskinState, backend: &mut WinitGraphicsBackend<GlesRenderer>) {
    if let Some(title) = state.emacs.take_title() {
        backend.window().set_title(&title);
    }

    if let Some(fullscreen) = state.emacs.take_pending_fullscreen() {
        if fullscreen {
            backend
                .window()
                .set_fullscreen(Some(winit_crate::window::Fullscreen::Borderless(None)));
        } else {
            backend.window().set_fullscreen(None);
        }
    }

    if let Some(maximize) = state.emacs.take_pending_maximize() {
        backend.window().set_maximized(maximize);
    }

    if let Some(enabled) = state.ime.take_ime_enabled() {
        backend.window().set_ime_allowed(enabled);
    }

    if let Some(status) = state.cursor.take_changed() {
        let window = backend.window();
        match status {
            CursorImageStatus::Named(icon) => {
                window.set_cursor_visible(true);
                window.set_cursor(winit_crate::window::Cursor::Icon(*icon));
            }
            // Surface cursors are software-rendered in render_frame();
            // hide the host cursor so it doesn't overlap.
            CursorImageStatus::Hidden | CursorImageStatus::Surface(_) => {
                window.set_cursor_visible(false);
            }
        }
    }
}

fn build_layer_surface_elements(
    renderer: &mut GlesRenderer,
    output: &Output,
    scale: f64,
) -> Vec<CustomElement<GlesRenderer>> {
    use smithay::desktop::layer_map_for_output;
    use smithay::wayland::shell::wlr_layer::Layer;

    // Collect surface + location while holding the LayerMap guard,
    // then drop the guard before calling the renderer (avoids holding
    // a MutexGuard across GL operations).
    let surface_locs: Vec<_> = {
        let map = layer_map_for_output(output);
        [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background]
            .iter()
            .flat_map(|&layer| {
                map.layers_on(layer)
                    .rev()
                    .map(|s| {
                        let loc = map.layer_geometry(s).map(|g| g.loc).unwrap_or_default();
                        (s.wl_surface().clone(), loc)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };

    let mut elements = Vec::new();
    for (wl_surface, loc) in &surface_locs {
        let layer_elements: Vec<CustomElement<GlesRenderer>> = render_elements_from_surface_tree(
            renderer,
            wl_surface,
            loc.to_physical_precise_round(scale),
            scale,
            1.0,
            Kind::Unspecified,
        );
        elements.extend(layer_elements);
    }
    elements
}

fn render_frame(
    state: &mut EmskinState,
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    output: &Output,
    damage_tracker: &mut OutputDamageTracker,
) {
    let size = backend.window_size();

    if output.current_mode().map(|m| m.size) != Some(size) {
        output.change_current_state(Some(make_mode(size)), None, None, None);
    }

    let damage = Rectangle::from_size(size);

    {
        let Ok((renderer, mut framebuffer)) = backend.bind() else {
            tracing::error!("Failed to bind rendering backend, skipping frame");
            return;
        };

        // emskin's responsibility: produce the per-frame snapshot data and
        // feed it into `effect_core::render_workspace`. Effect-core owns the
        // smithay `render_output` call and all damage-tracking bookkeeping.
        let scale = output.current_scale().fractional_scale();
        let output_size_log: Size<i32, Logical> = size.to_f64().to_logical(scale).to_i32_round();
        // Effects paint into the non-exclusive zone — the space layer-shell
        // surfaces (e.g. the external bar) haven't claimed. Falls back to the
        // full output when no bar / no layer is mapped.
        let canvas = state
            .emacs_geometry()
            .unwrap_or_else(|| Rectangle::from_size(output_size_log));

        // Edge-detect Emacs connection and trigger `splash.dismiss` once.
        let emacs_now = state.emacs.has_main_surface();
        if emacs_now && !state.effects.last_emacs_connected {
            state.effects.splash.borrow_mut().dismiss();
        }
        state.effects.last_emacs_connected = emacs_now;

        // Non-effect elements: software cursor, layer shell surfaces, window
        // mirrors. emskin assembles these itself.
        let mut extras: Vec<CustomElement<GlesRenderer>> = Vec::new();

        // Software cursor (topmost of extras): used for Surface cursors
        // (GTK3/Emacs) that can't be forwarded via winit's CursorIcon API.
        state.cursor.ensure_alive();
        if let CursorImageStatus::Surface(surface) = state.cursor.status() {
            if let Some(pointer) = state.seat.get_pointer() {
                if let Err(e) = import_surface_tree(renderer, surface) {
                    tracing::warn!("cursor import_surface_tree failed: {e:?}");
                } else {
                    let cursor_pos = pointer.current_location();
                    let ctx = renderer.context_id();
                    with_states(surface, |data| {
                        let hotspot = data
                            .data_map
                            .get::<CursorImageSurfaceData>()
                            .map(|d| d.lock().unwrap().hotspot)
                            .unwrap_or_default();
                        let Some(rss) = data.data_map.get::<RendererSurfaceStateUserData>() else {
                            return;
                        };
                        let rss = rss.lock().unwrap();
                        let Some(texture) = rss.texture::<GlesTexture>(ctx.clone()).cloned() else {
                            return;
                        };
                        let view = rss.view();
                        let pos = (cursor_pos - hotspot.to_f64()).to_physical(scale);
                        extras.push(
                            TextureRenderElement::from_static_texture(
                                Id::from_wayland_resource(surface),
                                ctx.clone(),
                                pos,
                                texture,
                                rss.buffer_scale(),
                                rss.buffer_transform(),
                                None, // alpha
                                view.map(|v| v.src),
                                view.map(|v| v.dst),
                                None, // damage
                                Kind::Cursor,
                            )
                            .into(),
                        );
                    });
                }
            }
        }

        // Layer surfaces + mirrors stacked below the chain output but above
        // the space's client windows.
        extras.extend(build_layer_surface_elements(renderer, output, scale));
        extras.extend(crate::mirror_render::build_mirror_elements(
            state, renderer, scale,
        ));

        let effect_ctx = effect_core::EffectCtx {
            cursor_pos: state.seat.get_pointer().map(|p| p.current_location()),
            canvas,
            scale,
            present_time: state.start_time.elapsed(),
        };

        match effect_core::render_workspace(
            output,
            renderer,
            &mut framebuffer,
            &state.workspace.active_space,
            &mut state.effects.chain,
            &effect_ctx,
            extras,
            damage_tracker,
            [1.0, 1.0, 1.0, 1.0],
        ) {
            Ok(outcome) => {
                if outcome.want_redraw {
                    state.needs_redraw = true;
                }
            }
            Err(e) => {
                tracing::error!("render_workspace failed: {e}");
                return;
            }
        }

        // Screencast hook. Fills the PBO here while the winit EGL surface
        // is still the current draw target; `map_texture` would detach it,
        // so the PNG write is deferred to after `backend.submit` below.
        state.recorder.capture_frame(
            renderer,
            &framebuffer,
            output,
            size,
            state.start_time.elapsed(),
        );
    }

    if let Err(e) = backend.submit(Some(&[damage])) {
        tracing::error!("frame submit failed: {e}");
        return;
    }

    // PBO → CPU → sink (PNG or ffmpeg stdin). Safe to map here because
    // swap_buffers already ran; `map_texture`'s make_current dance only
    // affects the next frame, which will re-`bind()` anyway.
    //
    // When a recording completes (either because the user stopped it or
    // because an internal condition like a framebuffer resize forced an
    // auto-stop), `finalize` returns a `FinishEvent`. We ping Emacs with
    // `RecordingStopped` so the elisp toggle flips back to nil even when
    // the stop wasn't user-initiated.
    if let Some(ev) = state.recorder.finalize(backend.renderer()) {
        state
            .ipc
            .send(crate::ipc::OutgoingMessage::RecordingStopped {
                path: ev.path.to_string_lossy().into_owned(),
                frames_written: ev.frames_written,
                duration_secs: ev.duration.as_secs_f64(),
                reason: ev.reason.as_str().to_string(),
            });
    }

    // Derive the overlay state from the recorder (single source of
    // truth): regardless of what triggered start/stop — user IPC,
    // auto-stop on resize, ffmpeg spawn failure, size mismatch — the
    // dot stays in lockstep with whatever the recorder actually is.
    let now = state.start_time.elapsed();
    let overlay_at = state.recorder.overlay_started_at(now);
    state
        .effects
        .recorder_overlay
        .borrow_mut()
        .set_active(overlay_at);

    // Recording ⇆ KeyCast linkage. Edge-trigger only so user toggles
    // (`SetKeyCast` IPC) outside a recording session aren't stomped.
    let recording_active = overlay_at.is_some();
    if recording_active != state.effects.last_recording_active {
        state.effects.last_recording_active = recording_active;
        state
            .effects
            .key_cast
            .borrow_mut()
            .set_enabled(recording_active);
        tracing::debug!(
            "key_cast auto-{} (recording {})",
            if recording_active { "on" } else { "off" },
            if recording_active {
                "started"
            } else {
                "stopped"
            }
        );
    }

    // Keep the render loop warm while a capture/recording is in progress —
    // otherwise an idle compositor stops generating frames and the video
    // would stall at whatever the last damage event was.
    if state.recorder.wants_continuous_frames() {
        state.needs_redraw = true;
    }
}

fn post_render(state: &mut EmskinState, output: &Output) {
    state.workspace.active_space.elements().for_each(|window| {
        window.send_frame(
            output,
            state.start_time.elapsed(),
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        )
    });

    // Layer surfaces: send frame callbacks and clean up dead ones.
    {
        use smithay::desktop::layer_map_for_output;
        let mut map = layer_map_for_output(output);
        let layers: Vec<_> = map.layers().cloned().collect();
        map.cleanup();
        drop(map);
        let elapsed = state.start_time.elapsed();
        for layer in &layers {
            layer.send_frame(output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    state.workspace.active_space.refresh();
    state.wl.popups.cleanup();

    if let Err(e) = state.display_handle.flush_clients() {
        tracing::warn!("flush_clients failed: {}", e);
    }
}

pub fn init_winit(
    event_loop: &mut EventLoop<EmskinState>,
    state: &mut EmskinState,
    fullscreen: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let attributes = winit_crate::window::Window::default_attributes()
        .with_inner_size(winit_crate::dpi::LogicalSize::new(1280.0, 800.0))
        .with_title("Emacs")
        .with_visible(true);
    let (mut backend, winit) = winit::init_from_attributes(attributes)?;
    if fullscreen {
        backend
            .window()
            .set_fullscreen(Some(winit_crate::window::Fullscreen::Borderless(None)));
        state.emacs.request_fullscreen(true);
    } else {
        backend.window().set_maximized(true);
    }

    let mode = make_mode(backend.window_size());

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<EmskinState>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    state.workspace.active_space.map_output(&output, (0, 0));

    init_dmabuf(&mut backend, state);

    state.backend = Some(backend);

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| {
            // Temporarily take backend out of state to avoid double &mut borrow
            // (the event handlers need &mut state AND &mut backend simultaneously).
            let mut backend = state
                .backend
                .take()
                .expect("backend missing in winit callback");
            match event {
                WinitEvent::Resized { size, scale_factor } => {
                    let int_scale = scale_factor.ceil() as i32;
                    tracing::info!(
                        "Host resized: {}x{} scale={} (int={})",
                        size.w,
                        size.h,
                        scale_factor,
                        int_scale
                    );
                    output.change_current_state(
                        Some(make_mode(size)),
                        None,
                        Some(Scale::Fractional(scale_factor)),
                        None,
                    );
                    // LayerMap caches `non_exclusive_zone` inside its `zone`
                    // field — it only refreshes when `arrange()` runs. Without
                    // this call, effects and Emacs would keep seeing the old
                    // canvas after a winit resize.
                    {
                        let mut map = smithay::desktop::layer_map_for_output(&output);
                        map.arrange();
                    }

                    if state.emacs.size_settled() {
                        // Re-lays out every Emacs frame against the fresh
                        // non_exclusive_zone and broadcasts SurfaceSize — also
                        // sets needs_redraw.
                        state.relayout_emacs();
                    } else {
                        state.needs_redraw = true;
                    }
                }

                WinitEvent::Input(event) => {
                    state.process_input_event(event);
                    state.needs_redraw = true;
                }

                WinitEvent::Redraw => {
                    apply_pending_state(state, &mut backend);
                    // Clear needs_redraw BEFORE render_frame so `Effect::post_paint`
                    // (splash animation) can re-arm it for the next frame.
                    if state.needs_redraw {
                        state.needs_redraw = false;
                        render_frame(state, &mut backend, &output, &mut damage_tracker);
                    }
                    post_render(state, &output);
                    backend.window().request_redraw();
                }

                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }

                WinitEvent::Ime(event) => {
                    state
                        .ime
                        .on_host_ime_event(event, &state.seat, &state.apps, backend.window());
                    state.needs_redraw = true;
                }

                WinitEvent::Focus(focused) => {
                    if focused {
                        state.on_focus_enter();
                        // Release all stuck keys to prevent phantom modifiers
                        // after Alt+Tab round-trip (the host eats the release).
                        let Some(keyboard) = state.seat.get_keyboard() else {
                            state.backend = Some(backend);
                            return;
                        };
                        let pressed = keyboard.pressed_keys();
                        if !pressed.is_empty() {
                            tracing::debug!(
                                "Window regained focus, releasing {} stuck keys",
                                pressed.len()
                            );
                            let time = state.start_time.elapsed().as_millis() as u32;
                            for keycode in pressed {
                                let serial = SERIAL_COUNTER.next_serial();
                                keyboard.input::<(), _>(
                                    state,
                                    keycode,
                                    KeyState::Released,
                                    serial,
                                    time,
                                    |_, _, _| FilterResult::Forward,
                                );
                            }
                        }
                    } else {
                        state.on_focus_leave();
                    }
                    state.needs_redraw = true;
                }
            };
            state.backend = Some(backend);
        })?;

    Ok(())
}

fn init_dmabuf(backend: &mut WinitGraphicsBackend<GlesRenderer>, state: &mut EmskinState) {
    use smithay::backend::{egl::EGLDevice, renderer::ImportDma};
    use smithay::wayland::dmabuf::DmabufFeedbackBuilder;

    let dmabuf_formats = backend.renderer().dmabuf_formats();

    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node())
        .ok()
        .flatten();

    let global = match render_node.and_then(|node| {
        DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats.clone())
            .build()
            .ok()
            .map(|fb| (node, fb))
    }) {
        Some((node, feedback)) => {
            tracing::info!("DMA-BUF v4 initialized (render node: {node:?})");
            state
                .wl
                .dmabuf_state
                .create_global_with_default_feedback::<EmskinState>(
                    &state.display_handle,
                    &feedback,
                )
        }
        None => {
            tracing::info!("DMA-BUF v3 initialized (no render node or feedback build failed)");
            state
                .wl
                .dmabuf_state
                .create_global::<EmskinState>(&state.display_handle, dmabuf_formats)
        }
    };
    state.wl.dmabuf_global = Some(global);
}
