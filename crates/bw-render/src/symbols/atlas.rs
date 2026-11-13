//! The shared static mesh atlas: every symbol (kind × input count × style)
//! tessellated once into one vertex array, with a sub-range per symbol, plus
//! the lookup table the GPU uses to find an instance's sub-range.

use std::collections::HashMap;
use std::sync::LazyLock;

use super::{KIND_CODES, SymVertex, Symbol, SymbolKind, SymbolStyle, VARIANT_SLOTS};

/// Sub-range of the atlas vertex array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshRange {
    pub start: u32,
    pub count: u32,
}

/// Layout of the GPU info table (`u32` words), shared with the shaders:
/// `[0, 17)` per-bucket base of the culled body list (written by the GPU),
/// `[17, 34)` per-bucket vertex count of the indirect draws (max over input
/// variants and styles), then `(start, count)` pairs indexed by
/// `(style * KIND_CODES + code) * VARIANT_SLOTS + variant`.
pub const INFO_BASE: usize = 0;
pub const INFO_VCOUNT: usize = KIND_CODES as usize;
pub const INFO_TABLE: usize = 2 * KIND_CODES as usize;

/// Vertex index bits reserved for the vertex within a mesh; the bucket (kind
/// code) of an indirect draw lives above them (`first_vertex = code << 16`).
pub const LOCAL_BITS: u32 = 16;

#[derive(Debug)]
pub struct Atlas {
    vertices: Vec<SymVertex>,
    ranges: HashMap<(SymbolKind, SymbolStyle), MeshRange>,
    bounds: HashMap<(SymbolKind, SymbolStyle), ([f32; 2], [f32; 2])>,
    sizes: HashMap<SymbolKind, [f32; 2]>,
    info: Vec<u32>,
}

/// The process-wide atlas (built on first use, a few milliseconds).
#[must_use]
pub fn atlas() -> &'static Atlas {
    static ATLAS: LazyLock<Atlas> = LazyLock::new(Atlas::build);
    &ATLAS
}

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Atlas {
    #[must_use]
    pub fn build() -> Self {
        let mut vertices = Vec::new();
        let mut ranges = HashMap::new();
        let mut bounds = HashMap::new();
        let mut sizes = HashMap::new();
        for kind in SymbolKind::all() {
            for style in SymbolStyle::ALL {
                if style != SymbolStyle::Ansi && !kind.style_dependent() {
                    // Same drawing in both styles: share the range.
                    let r = ranges[&(kind, SymbolStyle::Ansi)];
                    ranges.insert((kind, style), r);
                    bounds.insert((kind, style), bounds[&(kind, SymbolStyle::Ansi)]);
                    continue;
                }
                let sym = Symbol::build(kind, style);
                sizes.insert(kind, sym.size);
                let mesh = sym.tessellate();
                let mut lo = [f32::MAX; 2];
                let mut hi = [f32::MIN; 2];
                for v in &mesh {
                    for k in 0..2 {
                        lo[k] = lo[k].min(v.pos[k]);
                        hi[k] = hi[k].max(v.pos[k]);
                    }
                }
                ranges.insert((kind, style), MeshRange { start: u32_of(vertices.len()), count: u32_of(mesh.len()) });
                bounds.insert((kind, style), (lo, hi));
                vertices.extend(mesh);
            }
        }
        let mut info = vec![0u32; INFO_TABLE + (2 * KIND_CODES * VARIANT_SLOTS * 2) as usize];
        for (&(kind, style), r) in &ranges {
            let Some(code) = kind.code() else { continue };
            let slots: Vec<u32> = match kind {
                SymbolKind::Gate(..) | SymbolKind::FlipFlop(..) | SymbolKind::Memory(..) => vec![kind.variant()],
                // Single variant: fill every slot so any packing finds it.
                _ => (0..VARIANT_SLOTS).collect(),
            };
            for variant in slots {
                let idx = INFO_TABLE + 2 * ((style.index() * KIND_CODES + code) * VARIANT_SLOTS + variant) as usize;
                info[idx] = r.start;
                info[idx + 1] = r.count;
                let vc = &mut info[INFO_VCOUNT + code as usize];
                *vc = (*vc).max(r.count);
            }
        }
        Self { vertices, ranges, bounds, sizes, info }
    }

    /// All vertices (uploaded once as a storage buffer).
    #[must_use]
    pub fn vertices(&self) -> &[SymVertex] {
        &self.vertices
    }

    /// GPU lookup table (see [`INFO_TABLE`]).
    #[must_use]
    pub fn gpu_info(&self) -> &[u32] {
        &self.info
    }

    #[must_use]
    pub fn range(&self, kind: SymbolKind, style: SymbolStyle) -> Option<MeshRange> {
        self.ranges.get(&(kind, style)).copied()
    }

    /// Vertices of one symbol.
    #[must_use]
    pub fn mesh(&self, kind: SymbolKind, style: SymbolStyle) -> &[SymVertex] {
        self.range(kind, style).map_or(&[], |r| &self.vertices[r.start as usize..(r.start + r.count) as usize])
    }

    /// Bounding box of a symbol's centre-line geometry (local units).
    #[must_use]
    pub fn bounds(&self, kind: SymbolKind, style: SymbolStyle) -> Option<([f32; 2], [f32; 2])> {
        self.bounds.get(&(kind, style)).copied()
    }

    /// Body size of a symbol (local units).
    #[must_use]
    pub fn size(&self, kind: SymbolKind) -> [f32; 2] {
        self.sizes.get(&kind).copied().unwrap_or([0.0; 2])
    }
}

#[cfg(test)]
mod tests {
    use bw_core::{GateOp, MAX_GATE_INPUTS, MIN_GATE_INPUTS};

    use super::*;

    #[test]
    fn every_kind_and_style_has_a_valid_range() {
        let a = atlas();
        let n = u32_of(a.vertices().len());
        for kind in SymbolKind::all() {
            for style in SymbolStyle::ALL {
                let r = a.range(kind, style).unwrap_or_else(|| panic!("{kind:?} {style:?} has no mesh"));
                assert!(r.count > 0 && r.count % 3 == 0, "{kind:?} {style:?} {r:?}");
                assert!(r.start + r.count <= n);
                assert!(r.count < (1 << LOCAL_BITS), "mesh too large for the vertex-index packing");
            }
        }
    }

    #[test]
    fn gpu_table_covers_every_component_variant() {
        let a = atlas();
        let info = a.gpu_info();
        let lookup = |style: SymbolStyle, code: u32, inputs: u32| {
            let idx = INFO_TABLE + 2 * ((style.index() * KIND_CODES + code) * VARIANT_SLOTS + inputs) as usize;
            MeshRange { start: info[idx], count: info[idx + 1] }
        };
        for style in SymbolStyle::ALL {
            for op in GateOp::ALL {
                for n in MIN_GATE_INPUTS..=MAX_GATE_INPUTS {
                    let k = SymbolKind::Gate(op, n);
                    let code = k.code().unwrap();
                    assert_eq!(Some(lookup(style, code, u32::from(n))), a.range(k, style));
                    assert!(info[INFO_VCOUNT + code as usize] >= a.range(k, style).unwrap().count);
                }
            }
            for k in [SymbolKind::Not, SymbolKind::Switch, SymbolKind::Clock, SymbolKind::Led, SymbolKind::TriState] {
                let code = k.code().unwrap();
                for variant in [0, 1] {
                    assert_eq!(Some(lookup(style, code, variant)), a.range(k, style));
                }
            }
            // Every flip-flop variant and memory size has its own mesh.
            for k in SymbolKind::all() {
                if let (SymbolKind::FlipFlop(..) | SymbolKind::Memory(..), Some(code)) = (k, k.code()) {
                    assert_eq!(Some(lookup(style, code, k.variant())), a.range(k, style), "{k:?}");
                    assert!(k.variant() < VARIANT_SLOTS);
                }
            }
        }
        // The variants of one kind code never collide.
        let mut seen = HashMap::new();
        for k in SymbolKind::all() {
            if let Some(code) = k.code() {
                assert!(seen.insert((code, k.variant()), k).is_none(), "{k:?} collides");
            }
        }
        println!("atlas: {} vertices ({} KiB)", a.vertices().len(), a.vertices().len() * 24 / 1024);
        for code in 0..KIND_CODES {
            assert!(info[INFO_VCOUNT + code as usize] > 0);
        }
    }
}
