//! Hand-rolled tessellation (no external crates): ear clipping for fills and
//! mitred quad strips for strokes.
//!
//! Stroke vertices carry the centre-line point plus an *extrude* vector of
//! unit half-width; the vertex shader scales it by the line's half-width for
//! the current zoom (`max(world width, minimum pixels)`). The same mesh
//! therefore draws crisp, never-vanishing lines at every zoom level, and the
//! palette icons (egui) and the app icon reuse it with their own widths.

use bytemuck::{Pod, Zeroable};

use super::path::{P, dist};
use super::{Layer, Role, Width};

/// One vertex of the shared symbol mesh atlas (GPU storage-buffer layout).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct SymVertex {
    /// Centre-line / fill point in symbol-local grid units.
    pub pos: [f32; 2],
    /// Offset per unit of half line width (zero for fills).
    pub ext: [f32; 2],
    /// Bits 0..4 role, 4..6 width class, 6..8 layer.
    pub attr: u32,
    /// Label anchor (x, y in 1/256 units, 16 bits each) for text that stays
    /// upright when the component rotates, or [`NO_ANCHOR`].
    pub anchor: u32,
}

/// [`SymVertex::anchor`] of geometry that rotates with its component.
pub const NO_ANCHOR: u32 = u32::MAX;

/// Packs a label anchor (0..256 units, 1/256 resolution).
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn pack_anchor(p: P) -> u32 {
    let q = |v: f32| (v * 256.0).round().clamp(0.0, 65_535.0) as u32;
    q(p[0]) | (q(p[1]) << 16)
}

impl SymVertex {
    #[must_use]
    pub fn role(&self) -> Role {
        Role::from_bits(self.attr & 0xf)
    }

    #[must_use]
    pub fn width(&self) -> Width {
        Width::from_bits((self.attr >> 4) & 3)
    }

    #[must_use]
    pub fn layer(&self) -> Layer {
        Layer::from_bits((self.attr >> 6) & 3)
    }

    /// Position for a given half line width (in the same units as `pos`).
    #[must_use]
    pub fn at(&self, half_width: f32) -> P {
        [self.pos[0] + self.ext[0] * half_width, self.pos[1] + self.ext[1] * half_width]
    }
}

#[must_use]
pub fn attr(role: Role, width: Width, layer: Layer) -> u32 {
    role as u32 | ((width as u32) << 4) | ((layer as u32) << 6)
}

/// Drops consecutive duplicates (and the closing duplicate of a closed path).
fn clean(points: &[P], closed: bool) -> Vec<P> {
    let mut v: Vec<P> = Vec::with_capacity(points.len());
    for &p in points {
        if v.last().is_none_or(|&q| dist(p, q) > 1e-5) {
            v.push(p);
        }
    }
    if closed && v.len() > 2 && dist(v[0], v[v.len() - 1]) <= 1e-5 {
        v.pop();
    }
    v
}

fn cross(o: P, a: P, b: P) -> f32 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

fn signed_area(v: &[P]) -> f32 {
    let n = v.len();
    (0..n).map(|i| v[i][0] * v[(i + 1) % n][1] - v[(i + 1) % n][0] * v[i][1]).sum::<f32>() * 0.5
}

/// Inside or on the boundary (a reflex vertex on a candidate diagonal
/// blocks the ear), excluding points that coincide with a corner.
fn inside_tri(p: P, a: P, b: P, c: P) -> bool {
    if [a, b, c].iter().any(|&q| dist(p, q) <= 1e-6) {
        return false;
    }
    let (d1, d2, d3) = (cross(a, b, p), cross(b, c, p), cross(c, a, p));
    d1 >= -1e-7 && d2 >= -1e-7 && d3 >= -1e-7
}

/// Triangulates a simple polygon (either winding) by ear clipping.
#[must_use]
pub fn triangulate(points: &[P]) -> Vec<[P; 3]> {
    let mut v = clean(points, true);
    if v.len() < 3 {
        return Vec::new();
    }
    if signed_area(&v) < 0.0 {
        v.reverse();
    }
    let mut idx: Vec<usize> = (0..v.len()).collect();
    let mut out = Vec::with_capacity(v.len().saturating_sub(2));
    let mut guard = 0;
    while idx.len() > 3 && guard < 10_000 {
        guard += 1;
        let n = idx.len();
        let mut clipped = false;
        for i in 0..n {
            let (ia, ib, ic) = (idx[(i + n - 1) % n], idx[i], idx[(i + 1) % n]);
            let (a, b, c) = (v[ia], v[ib], v[ic]);
            let cr = cross(a, b, c);
            if cr < -1e-7 {
                continue; // reflex
            }
            if cr.abs() <= 1e-7 {
                // Collinear: drop the middle point, no triangle.
                idx.remove(i);
                clipped = true;
                break;
            }
            let blocked = idx.iter().any(|&k| k != ia && k != ib && k != ic && inside_tri(v[k], a, b, c));
            if !blocked {
                out.push([a, b, c]);
                idx.remove(i);
                clipped = true;
                break;
            }
        }
        if !clipped {
            // Degenerate input: fall back to a fan of what is left.
            break;
        }
    }
    for k in 1..idx.len().saturating_sub(1) {
        out.push([v[idx[0]], v[idx[k]], v[idx[k + 1]]]);
    }
    out
}

/// Appends the fill triangles of a polygon.
pub fn fill(points: &[P], attr: u32, out: &mut Vec<SymVertex>) {
    for t in triangulate(points) {
        for p in t {
            out.push(SymVertex { pos: p, ext: [0.0; 2], attr, anchor: NO_ANCHOR });
        }
    }
}

fn norm(d: P) -> P {
    let l = (d[0] * d[0] + d[1] * d[1]).sqrt();
    if l < 1e-9 { [1.0, 0.0] } else { [d[0] / l, d[1] / l] }
}

/// Maximum miter length (in half-widths) before a joint is clamped.
const MITER_LIMIT: f32 = 4.0;

/// Appends a stroke along a polyline (square caps when open, mitred joins).
pub fn stroke(points: &[P], closed: bool, attr: u32, out: &mut Vec<SymVertex>) {
    let v = clean(points, closed);
    let n = v.len();
    if n < 2 {
        return;
    }
    let segs = if closed { n } else { n - 1 };
    let dir: Vec<P> = (0..segs).map(|k| norm([v[(k + 1) % n][0] - v[k][0], v[(k + 1) % n][1] - v[k][1]])).collect();
    let normal = |d: P| [-d[1], d[0]];
    // Left / right extrude of each point.
    let mut ext: Vec<(P, P)> = Vec::with_capacity(n);
    for i in 0..n {
        let before = if i > 0 {
            Some(dir[i - 1])
        } else if closed {
            Some(dir[segs - 1])
        } else {
            None
        };
        let after = if i < segs { Some(dir[i]) } else { None };
        let e = match (before, after) {
            (Some(d0), Some(d1)) => {
                let (n0, n1) = (normal(d0), normal(d1));
                let m = [n0[0] + n1[0], n0[1] + n1[1]];
                if m[0].hypot(m[1]) < 1e-4 {
                    (n1, [-n1[0], -n1[1]])
                } else {
                    let m = norm(m);
                    let k = (1.0 / (m[0] * n1[0] + m[1] * n1[1]).max(1e-3)).min(MITER_LIMIT);
                    let e = [m[0] * k, m[1] * k];
                    (e, [-e[0], -e[1]])
                }
            }
            (None, Some(d)) => {
                let nn = normal(d);
                ([nn[0] - d[0], nn[1] - d[1]], [-nn[0] - d[0], -nn[1] - d[1]])
            }
            (Some(d), None) => {
                let nn = normal(d);
                ([nn[0] + d[0], nn[1] + d[1]], [-nn[0] + d[0], -nn[1] + d[1]])
            }
            (None, None) => ([0.0; 2], [0.0; 2]),
        };
        ext.push(e);
    }
    let vx = |i: usize, left: bool| SymVertex {
        pos: v[i],
        ext: if left { ext[i].0 } else { ext[i].1 },
        attr,
        anchor: NO_ANCHOR,
    };
    for k in 0..segs {
        let (i, j) = (k, (k + 1) % n);
        out.extend_from_slice(&[vx(i, true), vx(i, false), vx(j, true), vx(j, true), vx(i, false), vx(j, false)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(ts: &[[P; 3]]) -> f32 {
        ts.iter().map(|t| cross(t[0], t[1], t[2]).abs() * 0.5).sum()
    }

    #[test]
    fn ear_clipping_covers_concave_polygons() {
        // An L shape (area 3) in both windings.
        let l = vec![[0.0, 0.0], [2.0, 0.0], [2.0, 1.0], [1.0, 1.0], [1.0, 2.0], [0.0, 2.0]];
        assert!((area(&triangulate(&l)) - 3.0).abs() < 1e-4);
        let mut r = l.clone();
        r.reverse();
        assert!((area(&triangulate(&r)) - 3.0).abs() < 1e-4);
        assert_eq!(triangulate(&l).len(), 4);
    }

    #[test]
    fn stroke_extrudes_are_unit_on_straight_runs() {
        let mut out = Vec::new();
        stroke(&[[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]], false, 0, &mut out);
        assert_eq!(out.len(), 12);
        // Interior point: pure normal of length 1.
        let mid = out.iter().find(|v| (v.pos[0] - 1.0).abs() < 1e-6).unwrap();
        assert!((mid.ext[0].hypot(mid.ext[1]) - 1.0).abs() < 1e-5);
    }
}
