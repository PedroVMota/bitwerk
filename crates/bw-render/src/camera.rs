use glam::{Mat4, Vec2, Vec3};

/// Minimum / maximum zoom in screen points per grid unit.
pub const MIN_ZOOM: f32 = 0.05;
pub const MAX_ZOOM: f32 = 200.0;

/// 2-D camera. Zooming/panning only changes these two values, which end up in
/// a single uniform; no instance data is touched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// World (grid-unit) coordinate at the centre of the viewport.
    pub center: Vec2,
    /// Screen points per grid unit.
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self { center: Vec2::ZERO, zoom: 16.0 }
    }
}

impl Camera {
    /// World → clip-space matrix for a viewport of `viewport` points (y down).
    #[must_use]
    pub fn view_proj(&self, viewport: Vec2) -> Mat4 {
        let vp = viewport.max(Vec2::ONE);
        Mat4::from_scale(Vec3::new(2.0 * self.zoom / vp.x, -2.0 * self.zoom / vp.y, 1.0))
            * Mat4::from_translation(Vec3::new(-self.center.x, -self.center.y, 0.0))
    }

    /// Converts a point relative to the viewport's top-left (in points) to world.
    #[must_use]
    pub fn screen_to_world(&self, p: Vec2, viewport: Vec2) -> Vec2 {
        self.center + (p - viewport * 0.5) / self.zoom
    }

    #[must_use]
    pub fn world_to_screen(&self, w: Vec2, viewport: Vec2) -> Vec2 {
        (w - self.center) * self.zoom + viewport * 0.5
    }

    /// Pans by a screen-space delta (points).
    pub fn pan_by(&mut self, delta: Vec2) {
        self.center -= delta / self.zoom;
    }

    /// Zooms by `factor` keeping the world point under `cursor` fixed.
    pub fn zoom_at(&mut self, cursor: Vec2, factor: f32, viewport: Vec2) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let before = self.screen_to_world(cursor, viewport);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.screen_to_world(cursor, viewport);
        self.center += before - after;
    }

    /// Visible world rectangle `(min, max)`.
    #[must_use]
    pub fn visible_rect(&self, viewport: Vec2) -> (Vec2, Vec2) {
        let half = viewport * 0.5 / self.zoom;
        (self.center - half, self.center + half)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_keeps_cursor_point_fixed() {
        let vp = Vec2::new(800.0, 600.0);
        let mut c = Camera::default();
        let cursor = Vec2::new(100.0, 450.0);
        let w = c.screen_to_world(cursor, vp);
        c.zoom_at(cursor, 3.7, vp);
        assert!((c.screen_to_world(cursor, vp) - w).length() < 1e-3);
        let back = c.world_to_screen(w, vp);
        assert!((back - cursor).length() < 1e-2);
    }

    #[test]
    fn view_proj_maps_visible_rect_to_ndc() {
        let vp = Vec2::new(800.0, 600.0);
        let c = Camera { center: Vec2::new(10.0, 5.0), zoom: 20.0 };
        let (min, max) = c.visible_rect(vp);
        let m = c.view_proj(vp);
        let a = m.project_point3(min.extend(0.0));
        let b = m.project_point3(max.extend(0.0));
        assert!((a.x + 1.0).abs() < 1e-4 && (a.y - 1.0).abs() < 1e-4);
        assert!((b.x - 1.0).abs() < 1e-4 && (b.y + 1.0).abs() < 1e-4);
    }
}
