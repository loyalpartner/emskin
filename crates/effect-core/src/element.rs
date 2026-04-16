//! The `CustomElement` sum type produced by the render pipeline.
//!
//! Moved here from `emskin/src/winit.rs` so both effects and the compositor
//! host can speak the same element vocabulary.

use smithay::{
    backend::renderer::{
        element::{
            memory::MemoryRenderBufferRenderElement, solid::SolidColorRenderElement,
            surface::WaylandSurfaceRenderElement, texture::TextureRenderElement,
        },
        gles::GlesTexture,
        ImportAll, ImportMem, Renderer,
    },
    render_elements,
};

/// Blanket trait bundling renderer constraints for the `render_elements!` macro
/// (which cannot parse associated-type bounds like
/// `Renderer<TextureId = GlesTexture>`).
pub trait EmskinRenderer: ImportAll + ImportMem + Renderer<TextureId = GlesTexture> {}
impl<R: ImportAll + ImportMem + Renderer<TextureId = GlesTexture>> EmskinRenderer for R {}

render_elements! {
    pub CustomElement<R> where R: EmskinRenderer;
    Surface=WaylandSurfaceRenderElement<R>,
    Mirror=TextureRenderElement<GlesTexture>,
    Solid=SolidColorRenderElement,
    Label=MemoryRenderBufferRenderElement<R>,
}
