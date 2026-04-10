use std::time::Duration;

use smithay::{
    backend::{
        input::KeyState,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                memory::MemoryRenderBufferRenderElement, render_elements,
                solid::SolidColorRenderElement, texture::TextureRenderElement,
            },
            gles::{GlesRenderer, GlesTexture},
            utils::with_renderer_surface_state,
            ImportMem, Renderer, Texture,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::keyboard::FilterResult,
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Logical, Physical, Rectangle, Size, Transform, SERIAL_COUNTER},
};

use crate::EmskinState;

/// Blanket trait bundling renderer constraints for the `render_elements!` macro
/// (which cannot parse associated-type bounds like `Renderer<TextureId = GlesTexture>`).
trait EmskinRenderer: ImportMem + Renderer<TextureId = GlesTexture> {}
impl<R: ImportMem + Renderer<TextureId = GlesTexture>> EmskinRenderer for R {}

render_elements! {
    pub CustomElement<R> where R: EmskinRenderer;
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
}

/// Build TextureRenderElements for all mirrors by reading each surface layer's
/// (toplevel + popups) committed texture — no copy, no snapshot.
fn build_mirror_elements(
    state: &mut EmskinState,
    renderer: &mut GlesRenderer,
    scale: f64,
) -> Vec<CustomElement<GlesRenderer>> {
    let ctx = renderer.context_id();
    let mut elements = Vec::new();
    for app in state.apps.windows_mut() {
        if app.mirrors.is_empty() {
            continue;
        }
        let Some(source_geo) = app.geometry else {
            continue;
        };
        let src_size = source_geo.size.to_f64();
        let layers = app.surface_layers();

        // Iterate layers in reverse: popups first (higher z-order in smithay's
        // front-to-back damage tracker), then toplevel last (background).
        for (layer_idx, layer) in layers.iter().enumerate().rev() {
            if let Err(e) =
                smithay::backend::renderer::utils::import_surface_tree(renderer, &layer.surface)
            {
                tracing::warn!(
                    "import_surface_tree failed for wid={} layer={layer_idx}: {e:?}",
                    app.window_id
                );
                continue;
            }
            let ctx_clone = ctx.clone();
            let Some((texture, buf_scale, buf_transform, view_src)) =
                with_renderer_surface_state(&layer.surface, |rss| {
                    let tex = rss.texture::<GlesTexture>(ctx_clone).cloned()?;
                    let src = rss.view().map(|v| v.src);
                    Some((tex, rss.buffer_scale(), rss.buffer_transform(), src))
                })
                .flatten()
            else {
                continue;
            };

            for mv in app.mirrors.values_mut() {
                let m = mv.geometry;
                let Some(ratio) =
                    crate::apps::AppManager::aspect_fit_ratio(src_size, m.size.to_f64())
                else {
                    continue;
                };

                let layer_x = m.loc.x as f64 + layer.offset.x as f64 * ratio;
                let layer_y = m.loc.y as f64 + layer.offset.y as f64 * ratio;

                let (fit_w, fit_h) = if layer_idx == 0 {
                    (
                        (src_size.w * ratio).round() as i32,
                        (src_size.h * ratio).round() as i32,
                    )
                } else {
                    let tex_size = texture.size();
                    (
                        (tex_size.w as f64 * ratio / buf_scale as f64).round() as i32,
                        (tex_size.h as f64 * ratio / buf_scale as f64).round() as i32,
                    )
                };

                // Stable render ID: toplevel uses mv.render_id, popup layers
                // use pre-allocated IDs from mv.popup_render_ids (grown on demand).
                let render_id = if layer_idx == 0 {
                    mv.render_id.clone()
                } else {
                    let popup_idx = layer_idx - 1;
                    while mv.popup_render_ids.len() <= popup_idx {
                        mv.popup_render_ids
                            .push(smithay::backend::renderer::element::Id::new());
                    }
                    mv.popup_render_ids[popup_idx].clone()
                };

                let element = TextureRenderElement::from_static_texture(
                    render_id,
                    ctx.clone(),
                    smithay::utils::Point::<f64, Logical>::from((layer_x, layer_y))
                        .to_physical(scale),
                    texture.clone(),
                    buf_scale,
                    buf_transform,
                    None, // alpha
                    view_src,
                    Some((fit_w.max(1), fit_h.max(1)).into()),
                    None, // opaque_regions
                    smithay::backend::renderer::element::Kind::Unspecified,
                );
                elements.push(element.into());
            }
        }
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
        //   1. Skeleton labels  (text on top of everything, per user request)
        //   2. Skeleton borders
        //   3. Crosshair label + lines
        //   4. Mirror texture elements (popups → toplevel)
        let scale = output.current_scale().fractional_scale();
        let mut custom_elements: Vec<CustomElement<GlesRenderer>> = Vec::new();

        // Skeleton: topmost. Push labels first, borders second, so labels
        // end up above borders within the skeleton layer group.
        let output_size_log: Size<i32, Logical> = size.to_f64().to_logical(scale).to_i32_round();
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

        // Crosshair: above mirrors, below skeleton.
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

        // Mirrors: bottom of the custom layer stack.
        custom_elements.extend(build_mirror_elements(state, renderer, scale));

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

    state.space.refresh();
    state.popups.cleanup();
    if let Err(e) = state.display_handle.flush_clients() {
        tracing::warn!("flush_clients failed: {}", e);
    }
}

/// Resize only the Emacs toplevel; EAF app sizes come from Emacs via IPC.
fn resize_emacs_surface(state: &mut EmskinState, logical: Size<i32, Logical>) {
    let Some(ref emacs_surface) = state.emacs_surface else {
        return;
    };
    for window in state.space.elements() {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        if toplevel.wl_surface() != emacs_surface {
            continue;
        }
        toplevel.with_pending_state(|s| {
            s.size = Some(logical);
        });
        toplevel.send_pending_configure();
        return;
    }
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

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| {
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
                    }
                }

                WinitEvent::Input(event) => state.process_input_event(event),

                WinitEvent::Redraw => {
                    apply_pending_state(state, &mut backend);
                    render_frame(state, &mut backend, &output, &mut damage_tracker);
                    post_render(state, &output);
                    backend.window().request_redraw();
                }

                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }

                WinitEvent::Focus(focused) => {
                    if focused {
                        // Release all stuck keys to prevent phantom modifiers
                        // after Alt+Tab round-trip (the host eats the release).
                        let Some(keyboard) = state.seat.get_keyboard() else {
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
                }
            };
        })?;

    Ok(())
}
