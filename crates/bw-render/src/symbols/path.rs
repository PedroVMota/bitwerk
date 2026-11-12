//! Path helpers: flattening of arcs and Bézier curves into polylines.
//!
//! Every symbol is described in its own unrotated local frame, in grid units,
//! with the origin at the body's top-left corner and y pointing down.

use std::f32::consts::PI;

/// A point in symbol-local grid units.
pub type P = [f32; 2];

/// Points of a circular arc from angle `a0` to `a1` (radians, y down: 0 is
/// +x, π/2 is +y), both ends included.
#[must_use]
pub fn arc(c: P, r: f32, a0: f32, a1: f32, segs: usize) -> Vec<P> {
    let segs = segs.max(1);
    (0..=segs)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 / segs as f32;
            let a = a0 + (a1 - a0) * t;
            [c[0] + r * a.cos(), c[1] + r * a.sin()]
        })
        .collect()
}

/// A full circle as a closed polygon (first point not repeated).
#[must_use]
pub fn circle(c: P, r: f32, segs: usize) -> Vec<P> {
    let mut v = arc(c, r, 0.0, 2.0 * PI, segs);
    v.pop();
    v
}

/// Cubic Bézier, both ends included.
#[must_use]
pub fn cubic(p0: P, p1: P, p2: P, p3: P, segs: usize) -> Vec<P> {
    let segs = segs.max(1);
    (0..=segs)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 / segs as f32;
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            [a * p0[0] + b * p1[0] + c * p2[0] + d * p3[0], a * p0[1] + b * p1[1] + c * p2[1] + d * p3[1]]
        })
        .collect()
}

/// Quadratic Bézier, both ends included.
#[must_use]
pub fn quad(p0: P, p1: P, p2: P, segs: usize) -> Vec<P> {
    let segs = segs.max(1);
    (0..=segs)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 / segs as f32;
            let u = 1.0 - t;
            let (a, b, c) = (u * u, 2.0 * u * t, t * t);
            [a * p0[0] + b * p1[0] + c * p2[0], a * p0[1] + b * p1[1] + c * p2[1]]
        })
        .collect()
}

/// Axis-aligned rectangle as a closed polygon.
#[must_use]
pub fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<P> {
    vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
}

/// Rounded rectangle as a closed polygon.
#[must_use]
pub fn round_rect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32, segs: usize) -> Vec<P> {
    let mut v = Vec::new();
    v.extend(arc([x1 - r, y0 + r], r, -PI / 2.0, 0.0, segs));
    v.extend(arc([x1 - r, y1 - r], r, 0.0, PI / 2.0, segs));
    v.extend(arc([x0 + r, y1 - r], r, PI / 2.0, PI, segs));
    v.extend(arc([x0 + r, y0 + r], r, PI, 1.5 * PI, segs));
    v
}

/// Appends `b` to `a`, skipping `b`'s first point when it repeats `a`'s last.
pub fn join(a: &mut Vec<P>, b: &[P]) {
    let skip = usize::from(matches!((a.last(), b.first()), (Some(x), Some(y)) if dist(*x, *y) < 1e-5));
    a.extend_from_slice(&b[skip.min(b.len())..]);
}

#[must_use]
pub fn dist(a: P, b: P) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}
