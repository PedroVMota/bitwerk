#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::too_many_lines
)]
//! Headless GPU tests: render through the real pipelines offscreen and check
//! pixels (smoke test, GPU culling, incremental patches, streaming). They
//! skip (pass) if no adapter exists, e.g. on GPU-less CI machines.
//!
//! `stress_frame_cost` is the benchmark behind `make bench`.

use std::time::Instant;

use bw_core::{Circuit, ComponentId, ComponentKind, Edit, GridPos, Rotation};
use bw_render::{Camera, CanvasRenderer, Change, FrameView, NetLevels, Scene};
use glam::Vec2;

struct Levels {
    levels: Vec<u8>,
    comps: Vec<ComponentId>,
}

impl NetLevels for Levels {
    fn key(&self) -> u64 {
        1
    }
    fn revision(&self) -> u64 {
        1
    }
    fn levels(&self) -> &[u8] {
        &self.levels
    }
    fn net_components(&self) -> &[ComponentId] {
        &self.comps
    }
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn gpu() -> Option<Gpu> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
    Some(Gpu { device, queue })
}

/// Offscreen target (optionally multisampled) with CPU read-back.
struct Target {
    w: u32,
    h: u32,
    view: wgpu::TextureView,
    tex: wgpu::Texture,
    ms_view: Option<wgpu::TextureView>,
}

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl Target {
    fn new(g: &Gpu, w: u32, h: u32, msaa: u32) -> Self {
        let mk = |samples: u32, usage: wgpu::TextureUsages| {
            g.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage,
                view_formats: &[],
            })
        };
        let tex = mk(1, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let ms_view = (msaa > 1).then(|| {
            mk(msaa, wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&wgpu::TextureViewDescriptor::default())
        });
        Self { w, h, view, tex, ms_view }
    }

    /// prepare → encode (culling) → paint, submitted; optionally waits.
    /// Returns the CPU time of `prepare` (ms).
    fn frame(
        &self,
        g: &Gpu,
        r: &mut CanvasRenderer,
        scene: &mut Scene,
        view: &FrameView,
        nets: Option<&dyn NetLevels>,
        wait: bool,
    ) -> f64 {
        let t = Instant::now();
        r.prepare(&g.device, &g.queue, scene, view, nets);
        let prep = t.elapsed().as_secs_f64() * 1e3;
        let mut enc = g.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let _ = r.encode(&mut enc);
        {
            let (v, resolve) = match &self.ms_view {
                Some(ms) => (ms, Some(&self.view)),
                None => (&self.view, None),
            };
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: v,
                        depth_slice: None,
                        resolve_target: resolve,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::RED),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                })
                .forget_lifetime();
            let vp = view.viewport_points * view.pixels_per_point;
            pass.set_viewport(0.0, 0.0, vp.x.min(self.w as f32), vp.y.min(self.h as f32), 0.0, 1.0);
            r.paint(&mut pass);
        }
        g.queue.submit([enc.finish()]);
        if wait {
            g.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        }
        prep
    }

    fn read(&self, g: &Gpu) -> Vec<[u8; 4]> {
        let row_bytes = (self.w * 4).div_ceil(256) * 256;
        let out = g.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(row_bytes * self.h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = g.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &out,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: Some(self.h),
                },
            },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
        g.queue.submit([enc.finish()]);
        out.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
        g.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = out.get_mapped_range(..).unwrap();
        let mut px = Vec::with_capacity((self.w * self.h) as usize);
        for y in 0..self.h {
            for x in 0..self.w {
                let i = (y * row_bytes + x * 4) as usize;
                px.push([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            }
        }
        px
    }
}

fn view(center: Vec2, zoom: f32, w: u32, h: u32) -> FrameView {
    FrameView { camera: Camera { center, zoom }, viewport_points: Vec2::new(w as f32, h as f32), pixels_per_point: 1.0 }
}

/// Plain grid background (0.115, 0.12, 0.135); body fills are brighter.
fn is_grid(p: [u8; 4]) -> bool {
    p[0] < 40 && p[1] < 42 && p[2] < 46
}

#[test]
fn renders_switch_body_and_grid() {
    let Some(g) = gpu() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let t = Target::new(&g, 200, 200, 1);
    let mut circuit = Circuit::new();
    let sw = circuit.add_component(ComponentKind::Switch { on: true }, GridPos::new(0, 0), Rotation::R0).unwrap();
    let mut scene = Scene::build(&circuit, 1);
    let mut r = CanvasRenderer::new(&g.device, FORMAT, 1);
    let nets = Levels { levels: vec![1], comps: vec![sw] };
    let _ = t.frame(&g, &mut r, &mut scene, &view(Vec2::new(1.0, 1.0), 40.0, 200, 200), Some(&nets), true);
    let px = t.read(&g);
    let center = px[(100 * 200 + 100) as usize];
    let corner = px[(3 * 200 + 3) as usize];
    // Lit switch: accent green fill.
    assert!(center[1] > 180 && center[0] < 140, "center pixel {center:?}");
    // Grid background: dark, not the red clear colour (grid pass drew it).
    assert!(corner[0] < 60 && corner[1] < 60 && corner[2] < 60, "corner pixel {corner:?}");
}

/// Pixel close to an RGB colour given as 0..1 floats.
fn near(p: [u8; 4], c: [f32; 3], tol: i32) -> bool {
    (0..3).all(|k| (i32::from(p[k]) - (c[k] * 255.0).round() as i32).abs() <= tol)
}

/// Symbols are drawn from the mesh atlas: strokes and fills land where the
/// vector geometry says (an AND gate's bounding-box corner stays empty, the
/// IEC rectangle fills it), pin stubs reach the pins, rotation follows the
/// pin geometry, state comes from the net bits (lit LED, closed switch), and
/// far zoom falls back to quads. Set `BW_SYMBOL_PNG=<dir>` to also write the
/// rendered images (used for visual review).
#[test]
fn renders_symbols_with_strokes_where_expected() {
    use bw_core::{GateOp, PinRef};
    use bw_render::SymbolStyle;
    let Some(g) = gpu() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let mut c = Circuit::new();
    let add = |c: &mut Circuit, kind, x, y, rot| c.add_component(kind, GridPos::new(x, y), rot).unwrap();
    let and = add(&mut c, ComponentKind::Gate { op: GateOp::And, inputs: 2 }, 0, 0, Rotation::R0);
    let _nand = add(&mut c, ComponentKind::Gate { op: GateOp::Nand, inputs: 3 }, 6, 0, Rotation::R0);
    let or = add(&mut c, ComponentKind::Gate { op: GateOp::Or, inputs: 2 }, 12, 0, Rotation::R90);
    let _xor = add(&mut c, ComponentKind::Gate { op: GateOp::Xor, inputs: 4 }, 18, 0, Rotation::R0);
    let sw = add(&mut c, ComponentKind::Switch { on: true }, 0, 8, Rotation::R0);
    let led = add(&mut c, ComponentKind::Led, 6, 8, Rotation::R0);
    let sw_off = add(&mut c, ComponentKind::Switch { on: false }, 0, 12, Rotation::R0);
    let _clk = add(&mut c, ComponentKind::Clock { half_period: 2 }, 6, 12, Rotation::R0);
    let _not = add(&mut c, ComponentKind::Not, 12, 8, Rotation::R0);
    c.connect(PinRef { component: sw, pin: 0 }, PinRef { component: led, pin: 0 }, vec![]).unwrap();
    let mut scene = Scene::build(&c, 1);
    let nets = Levels { levels: vec![1, 0, 0], comps: vec![sw, sw_off, and] };
    let (w, h) = (960u32, 640u32);
    let t = Target::new(&g, w, h, 1);
    let zoom = 40.0;
    let v = view(Vec2::new(11.0, 7.5), zoom, w, h);
    let at = |px: &[[u8; 4]], wx: f32, wy: f32| {
        let s = v.camera.world_to_screen(Vec2::new(wx, wy), v.viewport_points);
        px[(s.y as u32 * w + s.x as u32) as usize]
    };
    let dump = |name: &str, px: &[[u8; 4]]| {
        if let Ok(dir) = std::env::var("BW_SYMBOL_PNG") {
            let raw: Vec<u8> = px.iter().flatten().copied().collect();
            let img = image::RgbaImage::from_raw(w, h, raw).unwrap();
            img.save(std::path::Path::new(&dir).join(name)).unwrap();
        }
    };
    let fill = [0.16, 0.17, 0.20];

    let mut r = CanvasRenderer::new(&g.device, FORMAT, 1);
    let _ = t.frame(&g, &mut r, &mut scene, &v, Some(&nets), true);
    assert!(r.draws_symbols());
    let px = t.read(&g);
    dump("symbols-ansi.png", &px);
    // AND (ANSI): arc tip is stroked, the inside is body fill, the
    // bounding-box corner beyond the arc is empty grid.
    assert!(!is_grid(at(&px, 2.9, 2.0)) && !near(at(&px, 2.9, 2.0), fill, 6), "AND tip stroke");
    assert!(near(at(&px, 1.5, 2.0), fill, 6), "AND fill {:?}", at(&px, 1.5, 2.0));
    assert!(is_grid(at(&px, 2.7, 0.5)), "corner outside the AND arc {:?}", at(&px, 2.7, 0.5));
    // Pin stubs reach the pins: input at x = 0.2, output at x = 3.8.
    assert!(!is_grid(at(&px, 0.2, 1.0)) && !is_grid(at(&px, 0.2, 3.0)), "AND input stubs");
    assert!(!is_grid(at(&px, 3.8, 2.0)), "AND output stub");
    // Between the input stubs there is nothing.
    assert!(is_grid(at(&px, 0.2, 2.0)));
    // OR rotated 90° clockwise: its output pin is at the bottom centre.
    let out = c.component(or).unwrap().pin_pos(2).unwrap();
    assert_eq!(out, GridPos::new(14, 4));
    assert!(!is_grid(at(&px, 14.0, 3.8)), "rotated output stub");
    assert!(is_grid(at(&px, 12.2, 3.8)), "rotated OR: no stub at the old corner");
    // Lit LED (driven by the closed switch): bright red lamp.
    let lamp = at(&px, 7.12, 9.3);
    assert!(lamp[0] > 200 && lamp[1] < 140, "lit LED {lamp:?}");
    // Closed switch: lever drawn in the high net colour along y = 1.
    let lever = at(&px, 0.9, 9.0);
    assert!(lever[1] > 200 && lever[0] < 120, "closed lever {lever:?}");
    // Open switch: no lever there, it is raised instead.
    assert!(at(&px, 0.9, 13.0)[1] <= 200, "open lever is not on the contact line");

    // IEC: the rectangle fills the corner the ANSI arc leaves empty.
    r.set_symbol_style(SymbolStyle::Iec);
    let _ = t.frame(&g, &mut r, &mut scene, &v, Some(&nets), true);
    let px = t.read(&g);
    dump("symbols-iec.png", &px);
    assert!(near(at(&px, 2.7, 1.8), fill, 6) || !is_grid(at(&px, 2.7, 1.8)));
    assert!(!is_grid(at(&px, 2.7, 0.5)), "IEC rectangle covers the corner {:?}", at(&px, 2.7, 0.5));

    // Far zoom: plain quads (the whole bounding box is filled).
    let far = view(Vec2::new(11.0, 7.5), 2.0, w, h);
    let _ = t.frame(&g, &mut r, &mut scene, &far, Some(&nets), true);
    assert!(!r.draws_symbols());
    let px = t.read(&g);
    let s = far.camera.world_to_screen(Vec2::new(3.6, 0.4), far.viewport_points);
    assert!(!is_grid(px[(s.y as u32 * w + s.x as u32) as usize]), "LOD quad covers the corner");
}

/// The example with every component kind (negative coordinates, rotations,
/// all input counts) renders through the symbol path at the app's MSAA.
#[test]
fn example_with_every_component_renders() {
    let Some(g) = gpu() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/all-components.bitwerk");
    let c = bw_io::load_from_path(&path).unwrap().circuit;
    let mut scene = Scene::build(&c, 1);
    let (w, h) = (900u32, 640u32);
    let t = Target::new(&g, w, h, 4);
    let mut r = CanvasRenderer::new(&g.device, FORMAT, 4);
    let mut empty = Scene::default();
    let v = view(Vec2::new(23.0, 7.0), 13.8, w, h);
    let _ = t.frame(&g, &mut r, &mut empty, &v, None, true);
    let _ = t.frame(&g, &mut r, &mut scene, &v, None, true);
    assert!(r.draws_symbols());
    let px = t.read(&g);
    if let Ok(dir) = std::env::var("BW_SYMBOL_PNG") {
        let raw: Vec<u8> = px.iter().flatten().copied().collect();
        image::RgbaImage::from_raw(w, h, raw).unwrap().save(std::path::Path::new(&dir).join("example.png")).unwrap();
    }
    // Every component's body centre region shows symbol pixels.
    for comp in c.components() {
        let b = comp.bounds();
        let mut hit = false;
        for dy in 0..=(b.max.y - b.min.y) * 4 {
            for dx in 0..=(b.max.x - b.min.x) * 4 {
                let wp = Vec2::new(b.min.x as f32 + dx as f32 / 4.0, b.min.y as f32 + dy as f32 / 4.0);
                let s = v.camera.world_to_screen(wp, v.viewport_points);
                if s.x >= 0.0 && s.y >= 0.0 && (s.x as u32) < w && (s.y as u32) < h {
                    hit |= !is_grid(px[(s.y as u32 * w + s.x as u32) as usize]);
                }
            }
        }
        assert!(hit, "{:?} at {:?} not drawn", comp.kind, comp.pos);
    }
}

/// A large scene viewed up close goes through the compute culling path; an
/// edit is patched in place and shows up in the very next frame; deleting
/// hides the slot and a new component reuses it.
#[test]
fn gpu_culling_and_incremental_patches() {
    let Some(g) = gpu() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    // 100 x 100 NOT gates (4 x 2 units) on a 10-unit pitch: 10k bodies.
    let mut c = Circuit::new();
    let mut ids = Vec::new();
    for y in 0..100 {
        for x in 0..100 {
            ids.push(c.add_component(ComponentKind::Not, GridPos::new(x * 10, y * 10), Rotation::R0).unwrap());
        }
    }
    let mut scene = Scene::build(&c, 1);
    let mut r = CanvasRenderer::new(&g.device, FORMAT, 1);
    let (w, h) = (100, 100);
    let t = Target::new(&g, w, h, 1);
    // Zoom 4: 25 units visible, around gate (50, 50) → culled.
    let v = view(Vec2::new(502.0, 501.0), 4.0, w, h);
    // Stream until everything is resident.
    for _ in 0..8 {
        let _ = t.frame(&g, &mut r, &mut scene, &v, None, true);
    }
    assert!(r.stream_progress().is_none());
    assert!(matches!(r.draw_mode(), bw_render::DrawMode::Culled(_)), "{:?}", r.draw_mode());
    let at = |px: &[[u8; 4]], wx: f32, wy: f32| {
        let s = v.camera.world_to_screen(Vec2::new(wx, wy), v.viewport_points);
        px[(s.y as u32 * w + s.x as u32) as usize]
    };
    let px = t.read(&g);
    assert!(!is_grid(at(&px, 502.0, 501.0)), "gate body expected at the centre");
    assert!(is_grid(at(&px, 507.0, 505.0)), "empty space between gates");

    // Move the centre gate by +5,+3 → old place empty, new place drawn.
    let mid = ids[50 * 100 + 50];
    c.apply(&Edit::MoveComponent(mid, GridPos::new(505, 503))).unwrap();
    scene.apply(&c, &[Change::Component(mid)], 2);
    let _ = t.frame(&g, &mut r, &mut scene, &v, None, true);
    assert!(r.last_upload_bytes() < 1024, "only the dirty slot is uploaded, got {}", r.last_upload_bytes());
    let px = t.read(&g);
    assert!(is_grid(at(&px, 502.0, 501.0)), "moved away");
    assert!(!is_grid(at(&px, 507.0, 504.0)), "moved here");

    // Delete it: slot hidden; a new component reuses the slot.
    let slot = scene.body_slot(mid).unwrap();
    c.apply(&Edit::RemoveComponent(mid)).unwrap();
    scene.apply(&c, &[Change::Component(mid)], 3);
    let _ = t.frame(&g, &mut r, &mut scene, &v, None, true);
    let px = t.read(&g);
    assert!(is_grid(at(&px, 507.0, 504.0)), "deleted");
    let add = Edit::AddComponent { kind: ComponentKind::Not, pos: GridPos::new(498, 497), rotation: Rotation::R0 };
    let bw_core::Applied::Component(new) = c.apply(&add).unwrap() else { panic!() };
    scene.apply(&c, &[Change::Component(new)], 4);
    assert_eq!(scene.body_slot(new), Some(slot), "free slot reused");
    let _ = t.frame(&g, &mut r, &mut scene, &v, None, true);
    let px = t.read(&g);
    assert!(!is_grid(at(&px, 499.0, 498.0)), "new component drawn");
    scene.validate(&c).unwrap();
}

/// A scene larger than one frame's upload budget is drawn partially while it
/// streams in and completely afterwards.
#[test]
fn large_scene_streams_over_several_frames() {
    let Some(g) = gpu() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let c = bw_core::stress::generate(20_000).unwrap();
    let mut scene = Scene::build(&c, 1);
    let bytes = scene.bodies().len() * 24 + scene.wires().len() * 24;
    assert!(bytes > bw_render::STREAM_BYTES_PER_FRAME);
    let mut r = CanvasRenderer::new(&g.device, FORMAT, 1);
    let t = Target::new(&g, 64, 64, 1);
    let v = view(Vec2::new(1280.0, 300.0), 0.02, 64, 64);
    let mut frames = 0;
    loop {
        let _ = t.frame(&g, &mut r, &mut scene, &v, None, false);
        assert!(r.last_upload_bytes() <= bw_render::STREAM_BYTES_PER_FRAME + 4096);
        frames += 1;
        if r.stream_progress().is_none() {
            break;
        }
        assert!(frames < 100);
    }
    assert_eq!(frames, bytes.div_ceil(bw_render::STREAM_BYTES_PER_FRAME));
    assert_eq!(r.draw_mode(), bw_render::DrawMode::All(scene.slot_count() as u32));
}

/// Frame-cost benchmark on the app's 100k stress circuit (`make bench`):
/// MSAA 4x like the app, at 1x (1280x800 px) and Retina (2560x1600 px), plus
/// the costs of loading (streaming) and of incremental edits.
#[test]
#[ignore = "benchmark; run manually"]
fn stress_frame_cost() {
    let Some(g) = gpu() else { return };
    let mut c = bw_core::stress::generate(100_000).unwrap();
    let t0 = Instant::now();
    let mut scene = Scene::build(&c, 1);
    println!(
        "scene build: {:.1} ms ({} instances, {:.1} MB)",
        t0.elapsed().as_secs_f64() * 1e3,
        scene.instance_count(),
        scene.slot_count() as f64 * 24.0 / 1e6
    );
    let (lo, hi) = scene.bounds();
    let center = (lo + hi) * 0.5;
    for (label, w, h, ppp) in [("1x 1280x800", 1280u32, 800u32, 1.0f32), ("2x 2560x1600", 2560, 1600, 2.0)] {
        let t = Target::new(&g, w, h, 4);
        let mut r = CanvasRenderer::new(&g.device, FORMAT, 4);
        let vp = Vec2::new(w as f32 / ppp * 0.7, h as f32 / ppp * 0.92);
        let ext = hi - lo;
        let fit = (vp.x / ext.x).min(vp.y / ext.y) * 0.95;
        let fv = |center: Vec2, zoom: f32| FrameView {
            camera: Camera { center, zoom },
            viewport_points: vp,
            pixels_per_point: ppp,
        };
        // Load: stream the scene in, timing each frame (CPU prepare + GPU).
        let mut load = Vec::new();
        let mut prep = Vec::new();
        loop {
            let s = Instant::now();
            prep.push(t.frame(&g, &mut r, &mut scene, &fv(center, fit), None, true));
            load.push(s.elapsed().as_secs_f64() * 1e3);
            if r.stream_progress().is_none() {
                break;
            }
        }
        println!(
            "{label} load: {} frames, worst frame {:.2} ms (prepare worst {:.2} ms)",
            load.len(),
            load.iter().copied().fold(0.0, f64::max),
            prep.iter().copied().fold(0.0, f64::max)
        );
        for (zl, zoom) in [("fit", fit), ("2x fit", fit * 2.0), ("4 pt/unit", 4.0), ("30 pt/unit", 30.0)] {
            let mut times = Vec::new();
            for f in 0..60 {
                let s = Instant::now();
                let _ = t.frame(&g, &mut r, &mut scene, &fv(center + Vec2::new(f as f32 * 0.5, 0.0), zoom), None, true);
                if f > 2 {
                    times.push(s.elapsed().as_secs_f64() * 1e3);
                }
            }
            times.sort_by(f64::total_cmp);
            let avg = times.iter().sum::<f64>() / times.len() as f64;
            println!(
                "{label} {zl:>10} (zoom {zoom:.3}): avg {avg:.2} ms, p50 {:.2}, max {:.2} ms, {:?}",
                times[times.len() / 2],
                times[times.len() - 1],
                r.draw_mode()
            );
        }
        // Edits: move a component (and its wires) per frame, patch + draw.
        let ids: Vec<ComponentId> = c.components().map(|k| k.id).step_by(97).take(200).collect();
        let mut apply = Vec::new();
        let mut frame = Vec::new();
        for (i, &id) in ids.iter().enumerate() {
            let s = Instant::now();
            let pos = c.component(id).unwrap().pos.offset(0, 1);
            c.apply(&Edit::MoveComponent(id, pos)).unwrap();
            scene.apply(&c, &[Change::Component(id)], 2 + i as u64);
            apply.push(s.elapsed().as_secs_f64() * 1e3);
            let _ = t.frame(&g, &mut r, &mut scene, &fv(center, 6.0), None, true);
            frame.push(s.elapsed().as_secs_f64() * 1e3);
        }
        println!(
            "{label} 200 edits: model+scene apply avg {:.3} ms / max {:.3} ms; edit frame (apply+upload+draw) avg {:.2} ms / max {:.2} ms",
            apply.iter().sum::<f64>() / apply.len() as f64,
            apply.iter().copied().fold(0.0, f64::max),
            frame.iter().sum::<f64>() / frame.len() as f64,
            frame.iter().copied().fold(0.0, f64::max),
        );
    }
    // Deletions: the model scans every wire (O(wires)), the scene does not.
    let ids: Vec<ComponentId> = c.components().map(|k| k.id).step_by(1009).take(50).collect();
    let (mut model, mut patch) = (Vec::new(), Vec::new());
    for (i, &id) in ids.iter().enumerate() {
        let s = Instant::now();
        c.apply(&Edit::RemoveComponent(id)).unwrap();
        model.push(s.elapsed().as_secs_f64() * 1e3);
        let s = Instant::now();
        scene.apply(&c, &[Change::Component(id)], 1000 + i as u64);
        patch.push(s.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "50 deletions: model avg {:.3} ms / max {:.3} ms; scene patch avg {:.3} ms / max {:.3} ms",
        model.iter().sum::<f64>() / 50.0,
        model.iter().copied().fold(0.0, f64::max),
        patch.iter().sum::<f64>() / 50.0,
        patch.iter().copied().fold(0.0, f64::max),
    );
}
