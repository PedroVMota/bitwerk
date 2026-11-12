//! Minimal CPU rasterizer for the symbol meshes (4 × 4 supersampling), used
//! to render the app icon from the same vector geometry as the canvas.
//!
//! Triangles are grouped into layers of one colour; each layer's coverage is
//! the *union* of its triangles' samples, so shared edges never show seams.

use std::f32::consts::PI;

use super::path::{P, arc, circle, round_rect};
use super::{Layer, Role, SymVertex, Symbol, SymbolKind, SymbolStyle, Width, level_color, tess};

const SS: usize = 4;

/// Straight-alpha RGBA float canvas.
pub struct Canvas {
    w: usize,
    h: usize,
    px: Vec<[f32; 4]>,
}

fn edge(a: P, b: P, p: P) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

impl Canvas {
    #[must_use]
    pub fn new(w: u32, h: u32) -> Self {
        let (w, h) = (w as usize, h as usize);
        Self { w, h, px: vec![[0.0; 4]; w * h] }
    }

    /// Composites one layer (triangles in pixel coordinates) over the canvas.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    pub fn fill(&mut self, tris: &[[P; 3]], color: [f32; 4]) {
        if tris.is_empty() {
            return;
        }
        let mut mask = vec![0u16; self.w * self.h];
        for t in tris {
            let area = edge(t[0], t[1], t[2]);
            if area.abs() < 1e-9 {
                continue;
            }
            let sgn = area.signum();
            let x0 = t.iter().map(|p| p[0]).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
            let y0 = t.iter().map(|p| p[1]).fold(f32::MAX, f32::min).floor().max(0.0) as usize;
            let x1 = (t.iter().map(|p| p[0]).fold(f32::MIN, f32::max).ceil() as usize).min(self.w);
            let y1 = (t.iter().map(|p| p[1]).fold(f32::MIN, f32::max).ceil() as usize).min(self.h);
            for y in y0..y1 {
                for x in x0..x1 {
                    let mut bits = 0u16;
                    for s in 0..SS * SS {
                        let p = [
                            x as f32 + ((s % SS) as f32 + 0.5) / SS as f32,
                            y as f32 + ((s / SS) as f32 + 0.5) / SS as f32,
                        ];
                        let inside = (0..3).all(|k| edge(t[k], t[(k + 1) % 3], p) * sgn >= 0.0);
                        if inside {
                            bits |= 1 << s;
                        }
                    }
                    mask[y * self.w + x] |= bits;
                }
            }
        }
        for (d, m) in self.px.iter_mut().zip(mask) {
            if m == 0 {
                continue;
            }
            let a = color[3] * m.count_ones() as f32 / (SS * SS) as f32;
            let out_a = a + d[3] * (1.0 - a);
            if out_a > 0.0 {
                for k in 0..3 {
                    d[k] = (color[k] * a + d[k] * d[3] * (1.0 - a)) / out_a;
                }
            }
            d[3] = out_a;
        }
    }

    /// 8-bit straight RGBA, row-major.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn into_rgba8(self) -> Vec<u8> {
        self.px.iter().flat_map(|p| p.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)).collect()
    }
}

/// Paints mesh vertices: `xf` maps local units to pixels, `half_width`
/// gives each width class in local units, `color` each vertex its colour
/// (`None` hides it). Consecutive triangles of one colour form one layer.
pub fn paint_mesh(
    canvas: &mut Canvas,
    mesh: &[SymVertex],
    xf: impl Fn(P) -> P,
    half_width: impl Fn(Width) -> f32,
    color: impl Fn(&SymVertex) -> Option<[f32; 4]>,
) {
    let mut layer: Vec<[P; 3]> = Vec::new();
    let mut current: Option<[f32; 4]> = None;
    for tri in mesh.chunks_exact(3) {
        let c = color(&tri[0]);
        if c != current {
            if let Some(cc) = current {
                canvas.fill(&layer, cc);
            }
            layer.clear();
            current = c;
        }
        if c.is_some() {
            layer.push([0, 1, 2].map(|i| xf(tri[i].at(half_width(tri[i].width())))));
        }
    }
    if let Some(cc) = current {
        canvas.fill(&layer, cc);
    }
}

fn shape_mesh(points: &[P], closed: bool, fill: bool) -> Vec<SymVertex> {
    let mut v = Vec::new();
    let a = tess::attr(Role::Outline, if fill { Width::None } else { Width::Outline }, Layer::Always);
    if fill {
        tess::fill(points, a, &mut v);
    } else {
        tess::stroke(points, closed, a, &mut v);
    }
    v
}

fn rgba(c: [f32; 3]) -> [f32; 4] {
    [c[0], c[1], c[2], 1.0]
}

/// The bitwerk app icon (original artwork): a NAND gate on a dark rounded
/// tile with a dotted grid, one high and one low input and a lit output.
/// Drawn on a 10 × 10 unit canvas from the same symbol mesh as the canvas.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn app_icon(size: u32) -> Vec<u8> {
    let s = size as f32 / 10.0;
    let mut cv = Canvas::new(size, size);
    let px = |o: P| move |p: P| [(p[0] + o[0]) * s, (p[1] + o[1]) * s];
    let fixed = |hw: f32| move |_: Width| hw;
    // Tile (macOS icon grid: content inset ~10 %).
    let tile = round_rect(0.55, 0.55, 9.45, 9.45, 2.0, 12);
    paint_mesh(&mut cv, &shape_mesh(&tile, true, true), px([0.0; 2]), fixed(0.0), |_| Some([0.11, 0.125, 0.15, 1.0]));
    // Subtle top highlight band.
    let mut band = arc([7.45, 2.55], 2.0, -PI / 2.0, 0.0, 12);
    band.extend([[9.45, 5.0], [0.55, 5.0]]);
    band.extend(arc([2.55, 2.55], 2.0, PI, 1.5 * PI, 12));
    paint_mesh(&mut cv, &shape_mesh(&band, true, true), px([0.0; 2]), fixed(0.0), |_| Some([1.0, 1.0, 1.0, 0.035]));
    paint_mesh(&mut cv, &shape_mesh(&tile, true, false), px([0.0; 2]), fixed(0.07), |_| Some([0.30, 0.33, 0.39, 1.0]));
    for gy in 1..=9 {
        for gx in 1..=9 {
            let (x, y) = (gx as f32, gy as f32);
            if (x - 5.0).abs() + (y - 5.0).abs() > 7.2 {
                continue;
            }
            let dot = shape_mesh(&circle([x, y], 0.07, 10), true, true);
            paint_mesh(&mut cv, &dot, px([0.0; 2]), fixed(0.0), |_| Some([0.27, 0.29, 0.34, 1.0]));
        }
    }
    // Wires: input A high, input B low, output high (NAND(1, 0) = 1).
    let wire = |cv: &mut Canvas, pts: &[P], level: u8| {
        paint_mesh(cv, &shape_mesh(pts, false, false), px([0.0; 2]), fixed(0.14), |_| Some(rgba(level_color(level))));
    };
    wire(&mut cv, &[[1.5, 4.0], [3.05, 4.0]], 1);
    wire(&mut cv, &[[1.5, 6.0], [3.05, 6.0]], 0);
    wire(&mut cv, &[[6.95, 5.0], [8.4, 5.0]], 1);
    for (c, level) in [([1.5, 4.0], 1), ([1.5, 6.0], 0), ([8.4, 5.0], 1)] {
        let d = shape_mesh(&circle(c, 0.3, 20), true, true);
        paint_mesh(&mut cv, &d, px([0.0; 2]), fixed(0.0), |_| Some(rgba(level_color(level))));
    }
    // The gate itself: the ANSI NAND symbol mesh, placed at (3, 3).
    let gate = Symbol::build(SymbolKind::Gate(bw_core::GateOp::Nand, 2), SymbolStyle::Ansi).tessellate();
    let outline = [0.62, 0.90, 1.0, 1.0];
    paint_mesh(
        &mut cv,
        &gate,
        px([3.0, 3.0]),
        |w| match w {
            Width::None => 0.0,
            Width::Thin => 0.08,
            Width::Outline | Width::Pin => 0.14,
        },
        |v| {
            Some(match v.role() {
                Role::Fill => [0.17, 0.21, 0.27, 1.0],
                // Input stubs continue their wires (upper input high).
                Role::PinIn => rgba(level_color(u8::from(v.pos[1] < 2.0))),
                Role::Outline | Role::Text => outline,
                Role::PinOut | Role::State => rgba(level_color(1)),
            })
        },
    );
    cv.into_rgba8()
}

#[cfg(test)]
mod tests {
    /// Every symbol in both styles, large, for visual review:
    /// `BW_SYMBOL_PNG=<dir> cargo test -p bw-render gallery -- --ignored`.
    #[test]
    #[ignore = "writes a review image"]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn gallery() {
        use super::super::{SymbolKind, SymbolStyle, Width, atlas, shade};
        let Ok(dir) = std::env::var("BW_SYMBOL_PNG") else { return };
        let kinds: Vec<SymbolKind> = SymbolKind::all()
            .into_iter()
            .filter(|k| !matches!(k, SymbolKind::Gate(_, n) if ![2, 3, 8].contains(n)))
            .collect();
        let (cell, scale, cols) = (190.0f32, 22.0f32, 10usize);
        let rows = kinds.len().div_ceil(cols) * 2;
        let (w, h) = ((cols as f32 * cell) as u32, (rows as f32 * cell) as u32);
        let mut cv = super::Canvas::new(w, h);
        let bg = vec![
            [[0.0, 0.0], [w as f32, 0.0], [w as f32, h as f32]],
            [[0.0, 0.0], [w as f32, h as f32], [0.0, h as f32]],
        ];
        cv.fill(&bg, [0.115, 0.12, 0.135, 1.0]);
        for (si, style) in SymbolStyle::ALL.into_iter().enumerate() {
            for (i, &k) in kinds.iter().enumerate() {
                let (cx, cy) = ((i % cols) as f32 * cell, ((i / cols) * 2 + si) as f32 * cell);
                let (lo, hi) = atlas().bounds(k, style).unwrap();
                let sc =
                    scale.min((cell - 20.0) / (hi[0] - lo[0]).max(0.5)).min((cell - 20.0) / (hi[1] - lo[1]).max(0.5));
                let off = [
                    cx + cell / 2.0 - f32::midpoint(lo[0], hi[0]) * sc,
                    cy + cell / 2.0 - f32::midpoint(lo[1], hi[1]) * sc,
                ];
                let level = u8::from(matches!(k, SymbolKind::Led | SymbolKind::Clock));
                super::paint_mesh(
                    &mut cv,
                    atlas().mesh(k, style),
                    |p| [off[0] + p[0] * sc, off[1] + p[1] * sc],
                    |wc: Width| wc.half_width(sc),
                    |v| {
                        v.layer().shows(level).then(|| {
                            let c = shade(v.role(), k.accent(), level);
                            [c[0], c[1], c[2], 1.0]
                        })
                    },
                );
            }
        }
        let img = image::RgbaImage::from_raw(w, h, cv.into_rgba8()).unwrap();
        img.save(std::path::Path::new(&dir).join("gallery.png")).unwrap();
    }

    #[test]
    fn app_icon_has_opaque_tile_and_transparent_corners() {
        let n = 64usize;
        let px = super::app_icon(64);
        assert_eq!(px.len(), n * n * 4);
        let at = |x: usize, y: usize| &px[(y * n + x) * 4..(y * n + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "corner is transparent");
        assert_eq!(at(n / 2, n / 8)[3], 255, "tile is opaque");
        // Something bright (the gate outline / lit wires) is drawn.
        assert!(px.chunks(4).any(|p| p[1] > 200));
    }
}
