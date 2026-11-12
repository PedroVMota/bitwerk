use serde::{Deserialize, Serialize};

/// A point on the integer editing grid (one unit = one grid cell).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct GridPos {
    pub x: i32,
    pub y: i32,
}

impl GridPos {
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Saturating addition: grid coordinates never wrap around.
    #[must_use]
    pub const fn offset(self, dx: i32, dy: i32) -> Self {
        Self { x: self.x.saturating_add(dx), y: self.y.saturating_add(dy) }
    }
}

/// Axis-aligned rectangle on the grid, `min` inclusive, `max` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridRect {
    pub min: GridPos,
    pub max: GridPos,
}

impl GridRect {
    #[must_use]
    pub fn from_points(a: GridPos, b: GridPos) -> Self {
        Self { min: GridPos::new(a.x.min(b.x), a.y.min(b.y)), max: GridPos::new(a.x.max(b.x), a.y.max(b.y)) }
    }

    #[must_use]
    pub fn contains(&self, p: GridPos) -> bool {
        p.x >= self.min.x && p.x < self.max.x && p.y >= self.min.y && p.y < self.max.y
    }
}
