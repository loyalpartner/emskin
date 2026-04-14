use std::time::Duration;

use smithay::{
    backend::{
        input::KeyState,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                memory::MemoryRenderBufferRenderElement,
                render_elements,
                solid::SolidColorRenderElement,
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                texture::TextureRenderElement,
                Id, Kind,
            },
            gles::{GlesRenderer, GlesTexture},
            utils::{import_surface_tree, RendererSurfaceStateUserData},
            ImportAll, ImportMem, Renderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::{
        keyboard::FilterResult,
        pointer::{CursorImageStatus, CursorImageSurfaceData},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_server::Resource,
    },
    utils::{Logical, Physical, Rectangle, Size, Transform, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        seat::WaylandFocus,
    },
};

use crate::EmskinState;

/// Blanket trait bundling renderer constraints for the `render_elements!` macro
/// (which cannot parse associated-type bounds like `Renderer<TextureId = GlesTexture>`).
trait EmskinRenderer: ImportAll + ImportMem + Renderer<TextureId = GlesTexture> {}
impl<R: ImportAll + ImportMem + Renderer<TextureId = GlesTexture>> EmskinRenderer for R {}

render_elements! {
    pub CustomElement<R> where R: EmskinRenderer;
    Surface=WaylandSurfaceRenderElement<R>,
    Mirror=TextureRenderElement<GlesTexture>,
    Solid=SolidColorRenderElement,
    Label=MemoryRenderBufferRenderElement<R>,
}

const REFRESH_RATE: i32 = 60_000;

fn make_mode(size: Size<i32, Physical>) -> Mode {
    Mode {
        size,
        refresh: REFRESH_RATE,
    }
}

fn apply_pending_state(state: &mut EmskinState, backend: &mut WinitGraphicsBackend<GlesRenderer>) {
    if let Some(title) = state.emacs_title.take() {
        backend.window().set_title(&title);
    }

    if let Some(fullscreen) = state.pending_fullscreen.take() {
        if fullscreen {
            backend
                .window()
                .set_fullscreen(Some(winit_crate::window::Fullscreen::Borderless(None)));
        } else {
            backend.window().set_fullscreen(None);
        }
    }

    if let Some(maximize) = state.pending_maximize.take() {
        backend.window().set_maximized(maximize);
    }

    if let Some(allowed) = state.focus.pending_ime_allowed.take() {
        backend.window().set_ime_allowed(allowed);
    }

    if state.cursor_changed {
        state.cursor_changed = false;
        let window = backend.window();
        match &state.cursor_status {
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

        // smithay's damage tracker renders elements via
        // `render_elements.iter().rev()`, so **the first element in the vec
        // is the topmost layer**. Layer order (top → bottom):
        //   1. Software cursor
        //   2. Splash screen (startup banner — fades out when Emacs connects)
        //   3. Workspace bar
        //   4. Skeleton labels / borders (debug overlay)
        //   5. Crosshair label + lines (debug overlay)
        //   6. Layer shell surfaces (Overlay → Top → Bottom → Background)
        //   7. Mirror texture elements (popups → toplevel)
        let scale = output.current_scale().fractional_scale();
        let mut custom_elements: Vec<CustomElement<GlesRenderer>> = Vec::new();
        let output_size_log: Size<i32, Logical> = size.to_f64().to_logical(scale).to_i32_round();

        // Software cursor: topmost layer. Used for Surface cursors (GTK3/Emacs)
        // that can't be forwarded to the host via winit's CursorIcon API.
        if let CursorImageStatus::Surface(ref surface) = state.cursor_status {
            if !surface.is_alive() {
                state.cursor_status = CursorImageStatus::default_named();
                state.cursor_changed = true;
            } else if let Some(pointer) = state.seat.get_pointer() {
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
                        custom_elements.push(
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

        // Splash screen: above all compositor content, below cursor.
        if !state.splash.is_done() {
            if state.emacs_surface.is_some() {
                state.splash.dismiss();
            }
            let (splash_solids, splash_labels) =
                state
                    .splash
                    .build_elements(renderer, output_size_log, scale);
            for l in splash_labels {
                custom_elements.push(l.into());
            }
            for s in splash_solids {
                custom_elements.push(s.into());
            }
        }

        // Workspace bar: absolute topmost layer (above skeleton).
        if state.bar_enabled && state.workspace_bar.visible() {
            let (bar_solids, bar_labels) =
                state
                    .workspace_bar
                    .build_elements(renderer, output_size_log, scale);
            for l in bar_labels {
                custom_elements.push(l.into());
            }
            for s in bar_solids {
                custom_elements.push(s.into());
            }
        }

        // Skeleton: topmost debug overlay. Push labels first, borders second, so labels
        // end up above borders within the skeleton layer group.
        let (skel_solids, skel_labels) =
            state
                .skeleton
                .build_elements(renderer, output_size_log, scale);
        for l in skel_labels {
            custom_elements.push(l.into());
        }
        for s in skel_solids {
            custom_elements.push(s.into());
        }

        // Crosshair: above layer surfaces, below skeleton.
        if let Some(pointer) = state.seat.get_pointer() {
            let cursor = pointer.current_location();
            let (solids, label) = state
                .crosshair
                .build_elements(renderer, cursor, size, scale);
            if let Some(l) = label {
                custom_elements.push(l.into());
            }
            for s in solids {
                custom_elements.push(s.into());
            }
        }

        // Layer surfaces: above mirrors, below debug overlays.
        custom_elements.extend(build_layer_surface_elements(renderer, output, scale));

        // Mirrors: bottom of the custom layer stack.
        custom_elements.extend(crate::mirror_render::build_mirror_elements(state, renderer, scale));

        let render_scale = 1.0;
        if let Err(e) = smithay::desktop::space::render_output::<_, CustomElement<GlesRenderer>, _, _>(
            output,
            renderer,
            &mut framebuffer,
            render_scale,
            0,
            [&state.space],
            &custom_elements,
            damage_tracker,
            [1.0, 1.0, 1.0, 1.0],
        ) {
            tracing::error!("render_output failed: {e}");
            return;
        }
    }

    if let Err(e) = backend.submit(Some(&[damage])) {
        tracing::error!("frame submit failed: {e}");
    }
}

fn post_render(state: &mut EmskinState, output: &Output) {
    state.space.elements().for_each(|window| {
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

    state.space.refresh();
    state.popups.cleanup();

    // Poll for X11 Emacs wl_surface (XWayland associates it asynchronously).
    if state.emacs_surface.is_none() {
        if let Some(ref win) = state.emacs_x11_window {
            if let Some(surface) = win.wl_surface().map(|s| s.into_owned()) {
                tracing::info!("X11 Emacs wl_surface resolved");
                state.emacs_surface = Some(surface);
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                if let Some(keyboard) = state.seat.get_keyboard() {
                    keyboard.set_focus(state, state.emacs_surface.clone(), serial);
                }
            }
        }
    }
    // Poll X11 cursor changes (emacs-gtk via XWayland).
    if let Some(ref mut tracker) = state.x11_cursor_tracker {
        tracker.dispatch();
        if let Some(icon) = tracker.take_pending() {
            state.cursor_status = smithay::input::pointer::CursorImageStatus::Named(icon);
            state.cursor_changed = true;
            state.needs_redraw = true;
        }
    }

    if let Err(e) = state.display_handle.flush_clients() {
        tracing::warn!("flush_clients failed: {}", e);
    }
}

/// Resize only the Emacs toplevel; embedded app sizes come from Emacs via IPC.
fn resize_emacs_surface(state: &mut EmskinState, logical: Size<i32, Logical>) {
    let geo = smithay::utils::Rectangle::new((0, 0).into(), logical);
    crate::state::resize_emacs_in_space(
        &mut state.space,
        &state.emacs_surface.clone(),
        &state.emacs_x11_window.clone(),
        geo,
    );
}

pub fn init_winit(
    event_loop: &mut EventLoop<EmskinState>,
    state: &mut EmskinState,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init()?;

    backend.window().set_title("Emacs");
    backend.window().set_maximized(true);

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

    state.space.map_output(&output, (0, 0));

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

                    if state.initial_size_settled {
                        let logical = size.to_f64().to_logical(scale_factor).to_i32_round();
                        resize_emacs_surface(state, logical);
                        // Also resize Emacs in all inactive workspaces.
                        let geo = smithay::utils::Rectangle::new((0, 0).into(), logical);
                        for ws in state.inactive_workspaces.values_mut() {
                            crate::state::resize_emacs_in_space(
                                &mut ws.space,
                                &ws.emacs_surface,
                                &ws.emacs_x11_window,
                                geo,
                            );
                        }
                    }
                    state.needs_redraw = true;
                }

                WinitEvent::Input(event) => {
                    state.process_input_event(event);
                    state.needs_redraw = true;
                }

                WinitEvent::Redraw => {
                    apply_pending_state(state, &mut backend);
                    // Keep rendering every frame during splash animation.
                    if !state.splash.is_done() {
                        state.needs_redraw = true;
                    }
                    if state.needs_redraw {
                        render_frame(state, &mut backend, &output, &mut damage_tracker);
                        state.needs_redraw = false;
                    }
                    post_render(state, &output);
                    backend.window().request_redraw();
                }

                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }

                WinitEvent::Ime(ime) => {
                    handle_ime_event(state, ime, backend.window());
                    state.needs_redraw = true;
                }

                WinitEvent::Focus(focused) => {
                    if focused {
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
                .dmabuf_state
                .create_global_with_default_feedback::<EmskinState>(
                    &state.display_handle,
                    &feedback,
                )
        }
        None => {
            tracing::info!("DMA-BUF v3 initialized (no render node or feedback build failed)");
            state
                .dmabuf_state
                .create_global::<EmskinState>(&state.display_handle, dmabuf_formats)
        }
    };
    state.dmabuf_global = Some(global);
}

fn handle_ime_event(
    state: &mut EmskinState,
    ime: winit_crate::event::Ime,
    window: &winit_crate::window::Window,
) {
    use smithay::wayland::text_input::TextInputSeat;
    use winit_crate::event::Ime;

    let ti = state.seat.text_input();

    // Sync cursor area so the host IME popup appears near the text cursor.
    // cursor_rectangle is surface-local; add the app's compositor position.
    if let Some(rect) = ti.cursor_rectangle() {
        let app_loc = ti
            .focus()
            .and_then(|s| state.apps.id_for_surface(&s))
            .and_then(|id| state.apps.get(id))
            .and_then(|app| app.geometry)
            .map(|g| g.loc)
            .unwrap_or_default();
        window.set_ime_cursor_area(
            winit_crate::dpi::LogicalPosition::new(
                (rect.loc.x + app_loc.x) as f64,
                (rect.loc.y + app_loc.y) as f64,
            ),
            winit_crate::dpi::LogicalSize::new(rect.size.w as f64, rect.size.h as f64),
        );
    }

    match ime {
        Ime::Preedit(text, cursor) => {
            let (begin, end) = cursor
                .map(|(b, e)| (b as i32, e as i32))
                .unwrap_or((-1, -1));
            ti.with_focused_text_input(|t, _| {
                t.preedit_string(Some(text.clone()), begin, end);
            });
            ti.done(false);
        }
        Ime::Commit(text) => {
            ti.with_focused_text_input(|t, _| {
                t.preedit_string(None, 0, 0);
                t.commit_string(Some(text.clone()));
            });
            ti.done(false);
        }
        Ime::Disabled => {
            ti.with_focused_text_input(|t, _| {
                t.preedit_string(None, 0, 0);
            });
            ti.done(false);
            ti.leave();
        }
        Ime::Enabled => {
            ti.enter();
        }
    }
}
