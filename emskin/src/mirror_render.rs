//! Mirror rendering — builds TextureRenderElements for scaled copies of
//! embedded app surfaces displayed in multiple Emacs windows.

use smithay::{
    backend::renderer::{
        element::{texture::TextureRenderElement, Id, Kind},
        gles::{GlesRenderer, GlesTexture},
        utils::{import_surface_tree, RendererSurfaceStateUserData},
        Renderer,
    },
    utils::{Logical, Point, Rectangle, Size, Transform},
    wayland::compositor::{with_surface_tree_downward, TraversalAction},
};

use crate::winit::CustomElement;
use crate::EmskinState;

/// Snapshot of one mapped surface within a layer's subsurface tree, in
/// source-logical coords. Collected once per layer so each mirror only has to
/// scale/translate (cheap), not re-walk the tree (expensive on GTK apps with
/// many subsurfaces).
struct SurfaceSnapshot {
    surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    /// Offset from the toplevel origin (already includes layer.offset for popups).
    offset: Point<f64, Logical>,
    view_src: Rectangle<f64, Logical>,
    view_dst: Size<i32, Logical>,
    texture: GlesTexture,
    buffer_scale: i32,
    buffer_transform: Transform,
}

/// Walk a layer's subsurface tree and collect one snapshot per mapped surface
/// with a texture. Offsets accumulate in source-logical space starting from
/// `layer.render_offset`, which already matches smithay's
/// `Space::render_location()` (canceling GTK CSD shadow padding).
fn collect_layer_surfaces(
    renderer: &mut GlesRenderer,
    layer: &crate::apps::SurfaceLayer,
) -> Vec<SurfaceSnapshot> {
    let ctx = renderer.context_id();
    let mut out: Vec<SurfaceSnapshot> = Vec::new();
    let initial =
        Point::<f64, Logical>::from((layer.render_offset.x as f64, layer.render_offset.y as f64));
    with_surface_tree_downward(
        &layer.surface,
        initial,
        |_, states, loc| {
            let mut loc = *loc;
            if let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() {
                if let Some(view) = data.lock().unwrap().view() {
                    loc.x += view.offset.x as f64;
                    loc.y += view.offset.y as f64;
                    return TraversalAction::DoChildren(loc);
                }
            }
            TraversalAction::SkipChildren
        },
        |surface, states, loc| {
            let mut loc = *loc;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return;
            };
            let data = data.lock().unwrap();
            let Some(view) = data.view() else { return };
            loc.x += view.offset.x as f64;
            loc.y += view.offset.y as f64;
            let Some(texture) = data.texture::<GlesTexture>(ctx.clone()).cloned() else {
                return;
            };
            out.push(SurfaceSnapshot {
                surface: surface.clone(),
                offset: loc,
                view_src: view.src,
                view_dst: view.dst,
                texture,
                buffer_scale: data.buffer_scale(),
                buffer_transform: data.buffer_transform(),
            });
        },
        |_, _, _| true,
    );
    out
}

/// Build TextureRenderElements for all mirrors. Handles clients (e.g. GTK /
/// Firefox) that render via `wl_subsurface` rather than attaching a buffer
/// directly to the toplevel.
pub fn build_mirror_elements(
    state: &mut EmskinState,
    renderer: &mut GlesRenderer,
    scale: f64,
) -> Vec<CustomElement<GlesRenderer>> {
    let ctx = renderer.context_id();
    let mut elements = Vec::new();
    for app in state.apps.windows() {
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
            if let Err(e) = import_surface_tree(renderer, &layer.surface) {
                tracing::warn!(
                    "import_surface_tree failed for wid={} layer={layer_idx}: {e:?}",
                    app.window_id
                );
                continue;
            }

            let snapshots = collect_layer_surfaces(renderer, layer);
            if snapshots.is_empty() {
                continue;
            }

            for (&view_id, mirror) in &app.mirrors {
                if mirror.workspace_id != state.active_workspace_id {
                    continue;
                }
                let Some(ratio) = crate::apps::AppManager::aspect_fit_ratio(
                    src_size,
                    mirror.geometry.size.to_f64(),
                ) else {
                    continue;
                };

                for snap in &snapshots {
                    let loc = Point::<f64, Logical>::from((
                        mirror.geometry.loc.x as f64 + snap.offset.x * ratio,
                        mirror.geometry.loc.y as f64 + snap.offset.y * ratio,
                    ));
                    let fit_w = (snap.view_dst.w as f64 * ratio).round() as i32;
                    let fit_h = (snap.view_dst.h as f64 * ratio).round() as i32;

                    // Stable per-(surface, mirror) ID so the damage tracker
                    // treats the same surface in different mirrors as distinct.
                    let render_id =
                        Id::from_wayland_resource(&snap.surface).namespaced(view_id as usize);

                    elements.push(
                        TextureRenderElement::from_static_texture(
                            render_id,
                            ctx.clone(),
                            loc.to_physical(scale),
                            snap.texture.clone(),
                            snap.buffer_scale,
                            snap.buffer_transform,
                            None,
                            Some(snap.view_src),
                            Some((fit_w.max(1), fit_h.max(1)).into()),
                            None,
                            Kind::Unspecified,
                        )
                        .into(),
                    );
                }
            }
        }
    }
    elements
}
