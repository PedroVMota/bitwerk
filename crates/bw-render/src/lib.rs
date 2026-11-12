//! `bw-render` — GPU canvas renderer for bitwerk circuits.
//!
//! * [`Camera`]: zoom/pan → one view-projection uniform.
//! * [`Scene`]: slot-stable instance arrays + R-tree, built off-thread once
//!   and then patched in place by edits ([`Scene::apply`]).
//! * [`CanvasRenderer`]: wgpu pipelines; instanced drawing with a small,
//!   constant number of draw calls (grid, wires, and either one call for
//!   all bodies as quads at far zoom or one indirect call per component kind
//!   with the symbol meshes), dirty-slot uploads, streamed scene loading and
//!   GPU (compute) frustum culling.
//! * [`symbols`]: original ANSI and IEC schematic symbols as vector paths,
//!   tessellated once into a shared static mesh atlas.
//!
//! The renderer is toolkit-agnostic: the application embeds it (e.g. through
//! an egui-wgpu paint callback) by calling [`CanvasRenderer::prepare`],
//! [`CanvasRenderer::encode`] and [`CanvasRenderer::paint`].

#![forbid(unsafe_code)]

pub mod camera;
pub mod renderer;
pub mod scene;
pub mod symbols;

pub use camera::Camera;
pub use renderer::{
    CanvasRenderer, DrawMode, FrameView, NetLevels, STREAM_BYTES_PER_FRAME, cull_shader_source, shader_source,
};
pub use scene::{
    BodyInstance, Change, Dirty, HIDDEN_KIND, Item, NO_NET, Scene, WireInstance, body_kind, kind_code, unpack_code,
};
pub use symbols::{SymbolKind, SymbolStyle};
