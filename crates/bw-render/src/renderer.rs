//! GPU side: pipelines, buffers, incremental uploads, culling and drawing.
//!
//! * Instance data lives in two storage buffers (bodies, wire segments)
//!   indexed by the scene's stable slots. Edits re-upload only the dirty
//!   slots (coalesced into runs); a new scene is streamed in over several
//!   frames with a fixed byte budget per frame, drawing what has arrived.
//! * Culling runs on the GPU: a small compute pass compacts the visible slots
//!   into index lists and fills the arguments of two indirect draws. The CPU
//!   does no per-object work while panning or zooming.
//! * Bodies are drawn as schematic symbols (see [`crate::symbols`]) from a
//!   static mesh atlas uploaded once. The culling pass groups the visible
//!   bodies by kind code; each kind is one indirect draw whose instances
//!   look up their own mesh (input count, style) and rotation in the vertex
//!   shader. At far zoom (below [`SYMBOL_LOD_PX`] pixels per unit) bodies
//!   fall back to one quad each, exactly as before.
//! * Draw calls per frame: grid + wires + either one body draw (quads) or
//!   one per kind code ([`crate::symbols::KIND_CODES`] = 10), independent
//!   of the number of objects.
//!
//! Call order per frame: [`CanvasRenderer::prepare`] (uploads; needs the
//! mutable scene), [`CanvasRenderer::encode`] (culling compute pass, before
//! the render pass), then [`CanvasRenderer::paint`] inside the render pass.

use std::borrow::Cow;

use bw_core::ComponentId;
use bytemuck::{Pod, Zeroable};
use glam::Vec2;

use crate::camera::Camera;
use crate::scene::{BodyInstance, NO_NET, Scene, WireInstance};
use crate::symbols::{self, KIND_CODES, SymVertex, SymbolStyle};

/// Below this many slots culling is not worth a compute pass: draw all.
const CULL_MIN_SLOTS: usize = 4_096;
/// The culled region is the view widened by this fraction per side.
const CULL_PADDING: f32 = 0.5;
/// Bytes of instance data streamed to the GPU per frame while a new scene
/// is loading (keeps every frame well inside its budget).
pub const STREAM_BYTES_PER_FRAME: usize = 1 << 20;
/// Dirty slots closer than this are uploaded as one run (fewer copies).
const RUN_GAP: u32 = 32;
const WG: u32 = 256;
/// Max workgroups per dispatch dimension (WebGPU default limit).
const MAX_GROUPS_X: u32 = 65_535;
/// Symbols are drawn from this many physical pixels per grid unit up (a
/// 2-input gate is then 20 px wide); below it bodies are plain quads.
pub const SYMBOL_LOD_PX: f32 = 5.0;
/// Culling columns: one per body bucket (kind code) plus the wires.
const NCOL: u64 = KIND_CODES as u64 + 1;
/// Indirect draw records: all bodies (quads), wires, one per bucket.
const ARGS_LEN: u64 = 2 + KIND_CODES as u64;

const RENDER_SRC: &str = concat!(
    include_str!("shaders/common.wgsl"),
    include_str!("shaders/instances.wgsl"),
    include_str!("shaders/grid.wgsl"),
    include_str!("shaders/body.wgsl"),
    include_str!("shaders/wire.wgsl"),
    include_str!("shaders/symbol.wgsl"),
);

const CULL_SRC: &str = concat!(include_str!("shaders/common.wgsl"), include_str!("shaders/cull.wgsl"));

/// Full WGSL source of the drawing shaders (exposed for validation tests).
#[must_use]
pub fn shader_source() -> &'static str {
    RENDER_SRC
}

/// Full WGSL source of the culling compute shaders.
#[must_use]
pub fn cull_shader_source() -> &'static str {
    CULL_SRC
}

const FLAG_NETS_VALID: u32 = 1;
const FLAG_CULLED: u32 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    viewport_px: [f32; 2],
    px_per_unit: f32,
    pixels_per_point: f32,
    flags: u32,
    net_count: u32,
    body_count: u32,
    wire_count: u32,
    cull_rect: [f32; 4],
    cull_groups: u32,
    style: u32,
    _pad: [u32; 2],
}

/// Logic levels of all nets, provided by the application (decoupled from the
/// simulator crate).
pub trait NetLevels {
    /// Changes whenever the levels change.
    fn key(&self) -> u64;
    /// Circuit revision the levels belong to.
    fn revision(&self) -> u64;
    /// One byte (0/1) per net.
    fn levels(&self) -> &[u8];
    /// Component driving each net (same length as `levels`).
    fn net_components(&self) -> &[ComponentId];
}

/// Camera and viewport of one frame.
#[derive(Debug, Clone, Copy)]
pub struct FrameView {
    pub camera: Camera,
    /// Canvas size in points.
    pub viewport_points: Vec2,
    pub pixels_per_point: f32,
}

/// What the last [`CanvasRenderer::prepare`] decided to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawMode {
    /// Every uploaded slot (this many).
    All(u32),
    /// The GPU-culled subset of this many uploaded slots.
    Culled(u32),
}

/// One storage buffer of instance records with a slot capacity.
struct SlotBuf {
    buf: wgpu::Buffer,
    /// Capacity in slots.
    cap: u64,
    stride: u64,
    label: &'static str,
}

impl SlotBuf {
    fn new(device: &wgpu::Device, label: &'static str, stride: u64, cap: u64, usage: wgpu::BufferUsages) -> Self {
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: (cap.max(4) * stride).max(16),
            usage,
            mapped_at_creation: false,
        });
        Self { buf, cap: cap.max(4), stride, label }
    }
}

fn storage_entry(binding: u32, read_only: bool, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Owns every GPU resource of the circuit canvas. Lives in egui-wgpu's
/// callback resources.
#[allow(clippy::struct_excessive_bools, reason = "independent flags of separate concerns")]
pub struct CanvasRenderer {
    grid_pipeline: wgpu::RenderPipeline,
    body_pipeline: wgpu::RenderPipeline,
    wire_pipeline: wgpu::RenderPipeline,
    symbol_pipeline: wgpu::RenderPipeline,
    cull_count: wgpu::ComputePipeline,
    cull_scan: wgpu::ComputePipeline,
    cull_write: wgpu::ComputePipeline,
    globals_layout: wgpu::BindGroupLayout,
    draw_layout: wgpu::BindGroupLayout,
    cull_layout: wgpu::BindGroupLayout,
    globals_bg: wgpu::BindGroup,
    draw_bg: wgpu::BindGroup,
    cull_bg: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    /// Static symbol mesh atlas and its lookup table (+ GPU-written bucket
    /// bases), see `symbols::atlas`.
    atlas_buf: wgpu::Buffer,
    info_buf: wgpu::Buffer,
    style: SymbolStyle,
    /// This frame draws symbol meshes (else quads).
    symbols: bool,

    // Net levels (one bit per body slot).
    net_buf: wgpu::Buffer,
    net_buf_words: u64,
    net_bits: Vec<u32>,
    net_key: Option<u64>,
    map_key: Option<(u64, usize, u64, u64)>,
    slot_of_net: Vec<u32>,
    net_valid: bool,
    net_count: u32,

    // Instance storage + culling outputs.
    bodies: SlotBuf,
    wires: SlotBuf,
    vis_bodies: SlotBuf,
    vis_wires: SlotBuf,
    groups: SlotBuf,
    args: wgpu::Buffer,
    /// Largest slot count a single storage binding can hold, per buffer.
    max_body_slots: u64,
    max_wire_slots: u64,

    // Upload state of the current scene.
    scene_id: Option<u64>,
    /// Slots `0..uploaded` are on the GPU (streaming watermark).
    uploaded_bodies: u32,
    uploaded_wires: u32,
    target_bodies: u32,
    target_wires: u32,
    warned_limit: bool,
    /// Uploaded counts the instance bind groups were created for.
    bound: Option<(u32, u32)>,

    // Per-frame decision.
    mode: DrawMode,
    /// Workgroups of this frame's culling dispatch (0: no compute pass).
    cull_groups: u32,
    /// Draw through the culled lists (possibly computed in an earlier frame).
    use_culled: bool,
    /// Padded world region the culled lists on the GPU cover. Only set by
    /// [`Self::encode`] when the compute pass is actually recorded: egui may
    /// run `ui` (and so `prepare`) twice in a frame and paint only the last
    /// pass, so a decision taken in `prepare` is not yet a computed list.
    cull_cache: Option<(Vec2, Vec2)>,
    /// Region this frame's compute pass will cover (moved to `cull_cache`
    /// by `encode`).
    cull_pending: Option<(Vec2, Vec2)>,
    /// Instance data changed since the lists were computed.
    cull_dirty: bool,
    last_upload_bytes: usize,
}

impl CanvasRenderer {
    #[must_use]
    #[allow(clippy::too_many_lines, reason = "linear GPU resource setup")]
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat, msaa_samples: u32) -> Self {
        let render_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bw-canvas"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(RENDER_SRC)),
        });
        let cull_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bw-cull"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(CULL_SRC)),
        });
        let all = wgpu::ShaderStages::VERTEX_FRAGMENT | wgpu::ShaderStages::COMPUTE;
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bw-canvas-globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: all,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage_entry(1, true, all),
            ],
        });
        let vs = wgpu::ShaderStages::VERTEX;
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bw-canvas-instances"),
            entries: &[
                storage_entry(0, true, vs),
                storage_entry(1, true, vs),
                storage_entry(2, true, vs),
                storage_entry(3, true, vs),
                storage_entry(4, true, vs),
                storage_entry(5, true, vs),
            ],
        });
        let cs = wgpu::ShaderStages::COMPUTE;
        let cull_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bw-cull"),
            entries: &[
                storage_entry(0, true, cs),
                storage_entry(1, true, cs),
                storage_entry(2, false, cs),
                storage_entry(3, false, cs),
                storage_entry(4, false, cs),
                storage_entry(5, false, cs),
                storage_entry(6, false, cs),
            ],
        });
        let draw_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bw-canvas"),
            bind_group_layouts: &[Some(&globals_layout), Some(&draw_layout)],
            immediate_size: 0,
        });
        let cull_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bw-cull"),
            bind_group_layouts: &[Some(&globals_layout), Some(&cull_layout)],
            immediate_size: 0,
        });

        let make = |label: &str, vs: &str, fs: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&draw_pl),
                vertex: wgpu::VertexState {
                    module: &render_module,
                    entry_point: Some(vs),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: msaa_samples.max(1),
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                fragment: Some(wgpu::FragmentState {
                    module: &render_module,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let grid_pipeline = make("bw-grid", "vs_grid", "fs_grid");
        let body_pipeline = make("bw-bodies", "vs_body", "fs_body");
        let wire_pipeline = make("bw-wires", "vs_wire", "fs_wire");
        let symbol_pipeline = make("bw-symbols", "vs_symbol", "fs_symbol");
        let make_cs = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&cull_pl),
                module: &cull_module,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let cull_count = make_cs("cs_count");
        let cull_scan = make_cs("cs_scan");
        let cull_write = make_cs("cs_write");

        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bw-globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let net_buf_words = 4;
        let net_buf = make_net_buf(device, net_buf_words);
        let globals_bg = make_bg(
            device,
            &globals_layout,
            "bw-canvas-globals",
            &[uniform.as_entire_binding(), net_buf.as_entire_binding()],
        );

        let inst_usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC;
        let bodies = SlotBuf::new(device, "bw-bodies", stride::<BodyInstance>(), 4, inst_usage);
        let wires = SlotBuf::new(device, "bw-wires", stride::<WireInstance>(), 4, inst_usage);
        let vis_usage = wgpu::BufferUsages::STORAGE;
        let vis_bodies = SlotBuf::new(device, "bw-vis-bodies", 4, 4, vis_usage);
        let vis_wires = SlotBuf::new(device, "bw-vis-wires", 4, 4, vis_usage);
        let groups = SlotBuf::new(device, "bw-cull-groups", 4 * NCOL, 4, vis_usage);
        let args = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bw-draw-args"),
            size: 16 * ARGS_LEN,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDIRECT,
            mapped_at_creation: false,
        });
        let atlas = symbols::atlas();
        let atlas_buf = init_buffer(
            device,
            "bw-symbol-atlas",
            bytemuck::cast_slice::<SymVertex, u8>(atlas.vertices()),
            wgpu::BufferUsages::STORAGE,
        );
        let info_buf = init_buffer(
            device,
            "bw-symbol-info",
            bytemuck::cast_slice::<u32, u8>(atlas.gpu_info()),
            wgpu::BufferUsages::STORAGE,
        );
        let limits = device.limits();
        let max_binding = limits.max_storage_buffer_binding_size.min(limits.max_buffer_size);
        let draw_bg = make_bg(
            device,
            &draw_layout,
            "bw-canvas-instances",
            &[
                whole(&bodies.buf),
                whole(&wires.buf),
                whole(&vis_bodies.buf),
                whole(&vis_wires.buf),
                whole(&atlas_buf),
                whole(&info_buf),
            ],
        );
        let cull_bg = make_bg(
            device,
            &cull_layout,
            "bw-cull",
            &[
                whole(&bodies.buf),
                whole(&wires.buf),
                whole(&vis_bodies.buf),
                whole(&vis_wires.buf),
                whole(&args),
                whole(&groups.buf),
                whole(&info_buf),
            ],
        );

        Self {
            grid_pipeline,
            body_pipeline,
            wire_pipeline,
            symbol_pipeline,
            cull_count,
            cull_scan,
            cull_write,
            globals_layout,
            draw_layout,
            cull_layout,
            globals_bg,
            draw_bg,
            cull_bg,
            uniform,
            atlas_buf,
            info_buf,
            style: SymbolStyle::default(),
            symbols: false,
            net_buf,
            net_buf_words,
            net_bits: Vec::new(),
            net_key: None,
            map_key: None,
            slot_of_net: Vec::new(),
            net_valid: false,
            net_count: 0,
            bodies,
            wires,
            vis_bodies,
            vis_wires,
            groups,
            args,
            max_body_slots: max_binding / stride::<BodyInstance>(),
            max_wire_slots: max_binding / stride::<WireInstance>(),
            scene_id: None,
            uploaded_bodies: 0,
            uploaded_wires: 0,
            target_bodies: 0,
            target_wires: 0,
            warned_limit: false,
            bound: None,
            mode: DrawMode::All(0),
            cull_groups: 0,
            use_culled: false,
            cull_cache: None,
            cull_pending: None,
            cull_dirty: true,
            last_upload_bytes: 0,
        }
    }

    /// Uploads whatever changed since the previous frame (dirty slots, the
    /// next chunk of a streaming scene, net levels) and the camera uniform.
    /// CPU cost is proportional to what changed, never to the scene size.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &mut Scene,
        view: &FrameView,
        nets: Option<&dyn NetLevels>,
    ) {
        self.sync_scene(device, queue, scene);
        self.update_nets(device, queue, scene, nets);
        self.update_globals(queue, scene, view);
    }

    /// Bytes written to instance buffers by the last `prepare`.
    #[must_use]
    pub fn last_upload_bytes(&self) -> usize {
        self.last_upload_bytes
    }

    /// Fraction of the current scene already on the GPU while it streams in
    /// (`None` once complete).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn stream_progress(&self) -> Option<f32> {
        let total = u64::from(self.target_bodies) + u64::from(self.target_wires);
        let done = u64::from(self.uploaded_bodies) + u64::from(self.uploaded_wires);
        (done < total).then(|| done as f32 / total as f32)
    }

    /// Selects the symbol style (a uniform: no mesh or instance changes).
    pub fn set_symbol_style(&mut self, style: SymbolStyle) {
        self.style = style;
    }

    #[must_use]
    pub fn symbol_style(&self) -> SymbolStyle {
        self.style
    }

    /// The last prepared frame draws symbol meshes (not far-zoom quads).
    #[must_use]
    pub fn draws_symbols(&self) -> bool {
        self.symbols
    }

    /// What was drawn last frame (for the stats overlay).
    #[must_use]
    pub fn draw_mode(&self) -> DrawMode {
        self.mode
    }

    fn clamp_slots(&mut self, n: usize, max: u64) -> u32 {
        let n = n as u64;
        if n > max && !self.warned_limit {
            self.warned_limit = true;
            tracing::warn!("scene exceeds the GPU storage limit ({n} > {max} slots); the excess is not drawn");
        }
        u32::try_from(n.min(max)).unwrap_or(u32::MAX)
    }

    fn sync_scene(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, scene: &mut Scene) {
        self.last_upload_bytes = 0;
        if self.scene_id != Some(scene.id()) {
            // A whole new scene: stream it in from slot 0.
            self.scene_id = Some(scene.id());
            self.uploaded_bodies = 0;
            self.uploaded_wires = 0;
            self.map_key = None;
            self.net_key = None;
            let _ = scene.take_dirty();
        }
        self.target_bodies = self.clamp_slots(scene.bodies().len(), self.max_body_slots);
        self.target_wires = self.clamp_slots(scene.wires().len(), self.max_wire_slots);
        self.ensure_capacity(device, queue);

        // Patch dirty slots already on the GPU; slots past the watermark are
        // streamed below from the current data anyway.
        let dirty = scene.take_dirty();
        self.last_upload_bytes +=
            write_runs(queue, &self.bodies.buf, scene.bodies(), dirty.bodies, self.uploaded_bodies);
        self.last_upload_bytes += write_runs(queue, &self.wires.buf, scene.wires(), dirty.wires, self.uploaded_wires);

        // Stream the next chunk (bodies first: they are what the eye finds).
        let budget = STREAM_BYTES_PER_FRAME;
        let budget =
            stream(queue, &self.bodies.buf, scene.bodies(), &mut self.uploaded_bodies, self.target_bodies, budget);
        let budget = stream(queue, &self.wires.buf, scene.wires(), &mut self.uploaded_wires, self.target_wires, budget);
        self.last_upload_bytes += STREAM_BYTES_PER_FRAME - budget;
        if self.last_upload_bytes > 0 {
            self.cull_dirty = true;
        }
        self.rebind(device);
    }

    /// Grows the instance buffers (geometrically) so the scene's slots fit.
    /// Already uploaded slots are copied GPU-side, so nothing is re-streamed.
    fn ensure_capacity(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let need_b = u64::from(self.target_bodies);
        let need_w = u64::from(self.target_wires);
        if need_b <= self.bodies.cap && need_w <= self.wires.cap {
            return;
        }
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("bw-grow") });
        let mut grow = |old: &mut SlotBuf, need: u64, kept: u32, max: u64| {
            if need <= old.cap {
                return;
            }
            let cap = (need + need / 4 + 1024).min(max.max(need));
            let usage = old.buf.usage();
            let new = SlotBuf::new(device, old.label, old.stride, cap, usage);
            let bytes = u64::from(kept) * old.stride;
            if bytes > 0 {
                enc.copy_buffer_to_buffer(&old.buf, 0, &new.buf, 0, bytes);
            }
            *old = new;
        };
        grow(&mut self.bodies, need_b, self.uploaded_bodies, self.max_body_slots);
        grow(&mut self.wires, need_w, self.uploaded_wires, self.max_wire_slots);
        queue.submit([enc.finish()]);
        // Culling outputs follow the instance capacity (GPU-written, no copy).
        self.vis_bodies = SlotBuf::new(device, "bw-vis-bodies", 4, self.bodies.cap, wgpu::BufferUsages::STORAGE);
        self.vis_wires = SlotBuf::new(device, "bw-vis-wires", 4, self.wires.cap, wgpu::BufferUsages::STORAGE);
        let groups = self.bodies.cap.max(self.wires.cap).div_ceil(u64::from(WG)) + 1;
        self.groups = SlotBuf::new(device, "bw-cull-groups", 4 * NCOL, groups, wgpu::BufferUsages::STORAGE);
        self.bound = None;
        self.cull_dirty = true;
    }

    /// Binds only the uploaded part of each buffer. Besides being exact,
    /// this keeps wgpu from zero-filling the not-yet-streamed tail of fresh
    /// buffers (and the driver from committing all their pages) in the
    /// first frame of a load.
    fn rebind(&mut self, device: &wgpu::Device) {
        let key = (self.uploaded_bodies, self.uploaded_wires);
        if self.bound == Some(key) {
            return;
        }
        self.bound = Some(key);
        let (nb, nw) = (u64::from(key.0.max(1)), u64::from(key.1.max(1)));
        let groups = nb.max(nw).div_ceil(u64::from(WG)).max(1);
        self.draw_bg = make_bg(
            device,
            &self.draw_layout,
            "bw-canvas-instances",
            &[
                part(&self.bodies, nb),
                part(&self.wires, nw),
                part(&self.vis_bodies, nb),
                part(&self.vis_wires, nw),
                whole(&self.atlas_buf),
                whole(&self.info_buf),
            ],
        );
        self.cull_bg = make_bg(
            device,
            &self.cull_layout,
            "bw-cull",
            &[
                part(&self.bodies, nb),
                part(&self.wires, nw),
                part(&self.vis_bodies, nb),
                part(&self.vis_wires, nw),
                self.args.as_entire_binding(),
                part(&self.groups, groups),
                whole(&self.info_buf),
            ],
        );
    }

    fn update_nets(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, scene: &Scene, nets: Option<&dyn NetLevels>) {
        let Some(nets) = nets else {
            self.net_valid = false;
            return;
        };
        let comps = nets.net_components();
        // Net index → body slot. Only depends on the simulator's compilation
        // and on the scene's slot assignment, so it is rebuilt once per edit.
        let map_key = (nets.revision(), comps.len(), scene.id(), scene.layout_version());
        let map_changed = self.map_key != Some(map_key);
        if map_changed {
            self.map_key = Some(map_key);
            self.slot_of_net.clear();
            self.slot_of_net.extend(comps.iter().map(|&id| scene.body_slot(id).unwrap_or(NO_NET)));
        }
        if !map_changed && self.net_key == Some(nets.key()) {
            return;
        }
        self.net_key = Some(nets.key());
        // Levels of an older revision stay meaningful: they are mapped
        // through component ids, so unchanged components keep their colour
        // while the simulator recompiles after an edit.
        let slots = scene.bodies().len();
        self.net_bits.clear();
        self.net_bits.resize(slots.div_ceil(32).max(1), 0);
        for (&lvl, &slot) in nets.levels().iter().zip(&self.slot_of_net) {
            if lvl != 0 && slot != NO_NET {
                if let Some(w) = self.net_bits.get_mut(slot as usize / 32) {
                    *w |= 1 << (slot % 32);
                }
            }
        }
        let words = self.net_bits.len() as u64;
        if words > self.net_buf_words {
            self.net_buf_words = words + words / 4;
            self.net_buf = make_net_buf(device, self.net_buf_words);
            self.globals_bg = make_bg(
                device,
                &self.globals_layout,
                "bw-canvas-globals",
                &[self.uniform.as_entire_binding(), self.net_buf.as_entire_binding()],
            );
        }
        queue.write_buffer(&self.net_buf, 0, bytemuck::cast_slice(&self.net_bits));
        self.net_count = u32::try_from(slots).unwrap_or(u32::MAX);
        self.net_valid = true;
    }

    fn update_globals(&mut self, queue: &wgpu::Queue, scene: &Scene, view: &FrameView) {
        let vp = view.viewport_points.max(Vec2::ONE);
        let px_per_unit = view.camera.zoom * view.pixels_per_point;
        // Cull against the view widened by the wire half-width (+ a pixel).
        let margin = 0.06f32.max(0.5 / px_per_unit) + 1.0 / px_per_unit;
        let (vmin, vmax) = view.camera.visible_rect(vp);
        let (vmin, vmax) = (vmin - Vec2::splat(margin), vmax + Vec2::splat(margin));
        let (smin, smax) = scene.bounds();
        let all_visible = vmin.x <= smin.x && vmin.y <= smin.y && vmax.x >= smax.x && vmax.y >= smax.y;
        let slots = self.uploaded_bodies.max(self.uploaded_wires);
        // Symbols are drawn per kind through the culled lists (which group
        // bodies by kind), even when everything is visible; far-zoom quads
        // keep the plain draw-everything path.
        self.symbols = px_per_unit >= SYMBOL_LOD_PX && self.uploaded_bodies > 0;
        let culled = self.symbols || (!all_visible && (slots as usize) >= CULL_MIN_SLOTS);
        // The visible lists are computed for a padded region and reused while
        // the view stays inside it (and nothing was uploaded): most panning
        // and zooming frames then need no compute pass at all.
        // Generous padding only pays off when few objects are visible; at
        // mid zoom it would multiply the drawn instances instead.
        let view_area = (vmax - vmin).x * (vmax - vmin).y;
        let scene_area = ((smax - smin).x * (smax - smin).y).max(1.0);
        let (pad_frac, max_ratio) = if view_area < 0.02 * scene_area { (CULL_PADDING, 9.0) } else { (0.1, 1.5) };
        let reuse = culled
            && !self.cull_dirty
            && self.cull_cache.is_some_and(|(cmin, cmax)| {
                let area = (cmax - cmin).x * (cmax - cmin).y;
                cmin.x <= vmin.x
                    && cmin.y <= vmin.y
                    && cmax.x >= vmax.x
                    && cmax.y >= vmax.y
                    && area <= view_area * max_ratio
            });
        self.cull_pending = None;
        if culled && !reuse {
            let pad = (vmax - vmin) * pad_frac;
            self.cull_pending = Some((vmin - pad, vmax + pad));
        }
        if !culled {
            self.cull_cache = None;
        }
        let (cmin, cmax) = self.cull_pending.or(self.cull_cache).unwrap_or((vmin, vmax));
        self.use_culled = culled;
        self.cull_groups = if culled && !reuse { slots.div_ceil(WG) } else { 0 };
        self.mode = if culled {
            DrawMode::Culled(self.uploaded_bodies + self.uploaded_wires)
        } else {
            DrawMode::All(self.uploaded_bodies + self.uploaded_wires)
        };
        let view_proj = view.camera.view_proj(vp);
        let mut flags = 0;
        if self.net_valid {
            flags |= FLAG_NETS_VALID;
        }
        if self.use_culled {
            flags |= FLAG_CULLED;
        }
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            viewport_px: (vp * view.pixels_per_point).to_array(),
            px_per_unit,
            pixels_per_point: view.pixels_per_point,
            flags,
            net_count: self.net_count,
            body_count: self.uploaded_bodies,
            wire_count: self.uploaded_wires,
            cull_rect: [cmin.x, cmin.y, cmax.x, cmax.y],
            cull_groups: self.cull_groups,
            style: self.style.index(),
            _pad: [0; 2],
        };
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&globals));
    }

    /// Encodes the culling compute pass (no-op when the lists on the GPU
    /// are still valid) and returns whether anything was recorded. Must be
    /// recorded before the render pass that calls [`Self::paint`], and the
    /// encoder must be submitted: the lists count as computed from here on
    /// (see [`Self::submit_culling`]).
    pub fn encode(&mut self, encoder: &mut wgpu::CommandEncoder) -> bool {
        if self.cull_groups == 0 {
            return false;
        }
        // From now on the lists cover this region (until data changes).
        self.cull_cache = self.cull_pending.take();
        self.cull_dirty = false;
        let groups = std::mem::take(&mut self.cull_groups);
        let x = groups.min(MAX_GROUPS_X);
        let y = groups.div_ceil(x);
        let mut pass =
            encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("bw-cull"), timestamp_writes: None });
        pass.set_bind_group(0, &self.globals_bg, &[]);
        pass.set_bind_group(1, &self.cull_bg, &[]);
        pass.set_pipeline(&self.cull_count);
        pass.dispatch_workgroups(x, y, 1);
        pass.set_pipeline(&self.cull_scan);
        pass.dispatch_workgroups(1, 1, 1);
        pass.set_pipeline(&self.cull_write);
        pass.dispatch_workgroups(x, y, 1);
        true
    }

    /// Records and submits the culling pass on its own command buffer, so
    /// it runs even when the host drops the frame's encoder (egui-wgpu does
    /// when no surface texture is available, e.g. while the window appears
    /// or is occluded). Queue order puts it before the frame's render pass.
    pub fn submit_culling(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if self.cull_groups == 0 {
            return;
        }
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("bw-cull") });
        if self.encode(&mut enc) {
            queue.submit([enc.finish()]);
        }
    }

    /// Records the draw calls into egui's render pass: grid, wires, then the
    /// bodies as quads (one call) or symbols (one indirect call per kind).
    pub fn paint(&self, pass: &mut wgpu::RenderPass<'static>) {
        pass.set_bind_group(0, &self.globals_bg, &[]);
        pass.set_bind_group(1, &self.draw_bg, &[]);
        pass.set_pipeline(&self.grid_pipeline);
        pass.draw(0..3, 0..1);
        if self.use_culled {
            pass.set_pipeline(&self.wire_pipeline);
            pass.draw_indirect(&self.args, 16);
            if self.symbols {
                pass.set_pipeline(&self.symbol_pipeline);
                for b in 0..u64::from(KIND_CODES) {
                    pass.draw_indirect(&self.args, 16 * (2 + b));
                }
            } else {
                pass.set_pipeline(&self.body_pipeline);
                pass.draw_indirect(&self.args, 0);
            }
        } else {
            if self.uploaded_wires > 0 {
                pass.set_pipeline(&self.wire_pipeline);
                pass.draw(0..6, 0..self.uploaded_wires);
            }
            if self.uploaded_bodies > 0 {
                pass.set_pipeline(&self.body_pipeline);
                pass.draw(0..6, 0..self.uploaded_bodies);
            }
        }
    }
}

fn stride<T>() -> u64 {
    std::mem::size_of::<T>() as u64
}

/// Writes the dirty slots below `limit`, merging nearby slots into runs.
fn write_runs<T: Pod>(queue: &wgpu::Queue, buf: &wgpu::Buffer, data: &[T], mut slots: Vec<u32>, limit: u32) -> usize {
    slots.retain(|&s| s < limit && (s as usize) < data.len());
    if slots.is_empty() {
        return 0;
    }
    slots.sort_unstable();
    slots.dedup();
    let mut bytes = 0;
    let mut i = 0;
    while i < slots.len() {
        let start = slots[i];
        let mut end = start;
        i += 1;
        while i < slots.len() && slots[i] - end <= RUN_GAP {
            end = slots[i];
            i += 1;
        }
        let run = &data[start as usize..=end as usize];
        let raw: &[u8] = bytemuck::cast_slice(run);
        queue.write_buffer(buf, u64::from(start) * stride::<T>(), raw);
        bytes += raw.len();
    }
    bytes
}

/// Uploads the next part of `data[*done..target]` within `budget` bytes;
/// returns the remaining budget.
fn stream<T: Pod>(
    queue: &wgpu::Queue,
    buf: &wgpu::Buffer,
    data: &[T],
    done: &mut u32,
    target: u32,
    budget: usize,
) -> usize {
    let per = std::mem::size_of::<T>();
    let left = target.saturating_sub(*done) as usize;
    let n = left.min(budget / per);
    if n == 0 {
        return budget;
    }
    let start = *done as usize;
    let raw: &[u8] = bytemuck::cast_slice(&data[start..start + n]);
    queue.write_buffer(buf, start as u64 * per as u64, raw);
    *done += u32::try_from(n).unwrap_or(0);
    budget - raw.len()
}

fn make_net_buf(device: &wgpu::Device, words: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("bw-net-bits"),
        size: words.max(4) * 4,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn init_buffer(device: &wgpu::Device, label: &str, data: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
    use wgpu::util::DeviceExt as _;
    let mut bytes = data.to_vec();
    bytes.resize(data.len().max(16).next_multiple_of(4), 0);
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: &bytes, usage })
}

fn whole(b: &wgpu::Buffer) -> wgpu::BindingResource<'_> {
    b.as_entire_binding()
}

/// The first `slots` records of a slot buffer.
fn part(b: &SlotBuf, slots: u64) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::Buffer(wgpu::BufferBinding {
        buffer: &b.buf,
        offset: 0,
        size: std::num::NonZeroU64::new(slots.min(b.cap) * b.stride),
    })
}

fn make_bg(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    label: &str,
    resources: &[wgpu::BindingResource<'_>],
) -> wgpu::BindGroup {
    let entries: Vec<wgpu::BindGroupEntry<'_>> = resources
        .iter()
        .enumerate()
        .map(|(i, r)| wgpu::BindGroupEntry { binding: u32::try_from(i).unwrap_or(0), resource: r.clone() })
        .collect();
    device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some(label), layout, entries: &entries })
}
