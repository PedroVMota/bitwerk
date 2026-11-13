//! `bw-io` — bitwerk project file format.
//!
//! Files are RON documents with a versioned header:
//!
//! ```ron
//! (
//!     format: "bitwerk",
//!     version: 3,
//!     name: "half adder",
//!     circuit: (components: [...], nodes: [...], segments: [...], memories: [...]),
//! )
//! ```
//!
//! Version history (older files are migrated on load, see [`migrate`]):
//! * **1** — gates, NOT, switch, clock, LED; two-valued logic.
//! * **2** — tri-state buffers, flip-flops, RAM/ROM and their contents
//!   (`memories`, compact hex runs, see [`bw_core::memenc`]); four-state
//!   logic (0, 1, Z, X) in the simulator.
//! * **3** — wiring is a graph: `nodes` (`Pin(...)` or `Free((x, y))`
//!   junctions / bends) and orthogonal `segments` between them, instead of
//!   point-to-point `wires`. Version 1 and 2 wires become an orthogonal path
//!   between their two pins.
//! * **4** — macros: a `macros` table of definitions (`id`, `name`, `pins`,
//!   `package`, `datasheet`, optional `doc_link` / `origin`, content `hash`
//!   and `body`, itself a circuit), macro tags (`Tag(dir: In, tristate:
//!   false)`) and instances (`Macro((3))`, a reference by definition id).
//!   Version 3 files load unchanged (no macros). See also [`bwmacro`] for
//!   single-macro files.
//! * **5** — power / ground (`Const`), sequence generators (`SeqGen`),
//!   7-segment displays (`SevenSeg`), numeric outputs (`Numeric`), text
//!   annotations (`Text` components and the circuit's `notes`), push-button
//!   and pulse switches (`Switch(on, mode: Momentary | Pulse)`), display
//!   colours (`color` on a component) and the simulation mode (`eval:
//!   Sequential`, the Digital Works compatible propagation). Every addition
//!   has a default, so version 4 files load unchanged; a version 4 file
//!   using any of them is refused.
//!
//! Loading treats every file as hostile input: the size is capped *before*
//! reading, RON nesting depth is bounded, object counts are bounded, and the
//! circuit is fully validated (see [`bw_core::Circuit`]) before it is returned.
//! Nothing in a file is ever executed.

#![forbid(unsafe_code)]

pub mod bwmacro;
pub mod datasheet;
pub mod dwm;
pub mod library;
pub mod memmap;

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use std::sync::Arc;

use bw_core::{
    Circuit, CircuitRaw, Component, ComponentKind, GridPos, LibraryRef, MacroDef, MacroId, MacroPin, MacroTable,
    MemoryRecord, Package, PinDir, PinRef,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Magic string identifying a bitwerk project.
pub const FORMAT_MAGIC: &str = "bitwerk";
/// Current (and highest understood) format version.
pub const FORMAT_VERSION: u32 = 5;
/// Oldest version that can still be loaded (and is migrated).
pub const OLDEST_VERSION: u32 = 1;
/// Maximum accepted file size in bytes.
pub const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
/// Maximum number of components in one project.
pub const MAX_COMPONENTS: usize = 1_000_000;
/// Maximum number of wire segments in one project.
pub const MAX_SEGMENTS: usize = 4_000_000;
/// Maximum number of wire graph nodes (pins in use, junctions and bends).
pub const MAX_WIRE_NODES: usize = 4_000_000;
/// Maximum number of junctions (solder dots) in one project.
pub const MAX_JUNCTIONS: usize = 1_000_000;
/// Maximum number of point-to-point wires in a version 1 or 2 file.
pub const MAX_LEGACY_WIRES: usize = 2_000_000;
/// Maximum macro nesting depth (see [`bw_core::MAX_MACRO_DEPTH`]).
pub const MAX_MACRO_DEPTH: usize = bw_core::MAX_MACRO_DEPTH as usize;
/// Maximum macro definitions in one file.
pub const MAX_MACROS: usize = bw_core::MAX_MACRO_DEFS;
/// Maximum size of the design with every macro expanded (refuses "macro
/// bombs", see [`bw_core::MAX_FLAT_COMPONENTS`]).
pub const MAX_FLAT_COMPONENTS: u64 = bw_core::MAX_FLAT_COMPONENTS;
/// Maximum project name length in bytes.
pub const MAX_NAME_LEN: usize = 256;
/// Maximum number of memory devices in one project (each holds at most
/// [`bw_core::MAX_MEMORY_WORDS`] words, so contents stay below 128 MiB even
/// before the file size limit applies).
pub const MAX_MEMORIES: usize = 4096;
/// RON parser recursion limit (bounds stack usage on hostile input).
const RON_RECURSION_LIMIT: usize = 64;

#[derive(Debug, Error)]
pub enum IoError {
    #[error("file is too large (limit {MAX_FILE_SIZE} bytes)")]
    TooLarge,
    #[error("not a bitwerk project file")]
    NotABitwerkFile,
    #[error("file was written by a newer version (format {found}, supported up to {FORMAT_VERSION})")]
    UnsupportedVersion { found: u32 },
    #[error("too many {what} (limit {limit})")]
    LimitExceeded { what: &'static str, limit: usize },
    #[error("malformed project file: {0}")]
    Parse(String),
    #[error("could not serialize project: {0}")]
    Serialize(String),
    #[error("format {version} file uses {feature}, which that version does not have")]
    FeatureNotInVersion { version: u32, feature: &'static str },
    #[error("legacy .dwm files are not yet supported")]
    LegacyNotSupported,
    #[error("Digital Works file: {0}")]
    Dwm(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// A bitwerk project (currently: one flat circuit).
///
/// Deliberately not `Serialize`/`Deserialize`: the only way in or out is
/// through the validated, versioned functions of this module.
#[derive(Debug, Clone, Default)]
pub struct Project {
    pub name: String,
    pub circuit: Circuit,
}

#[derive(Serialize)]
struct FileOut<'a> {
    format: &'a str,
    version: u32,
    name: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    macros: Vec<MacroRecordOut<'a>>,
    circuit: &'a Circuit,
}

/// On-disk macro definition (format 4, `.bwmacro`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MacroRecordIn {
    id: MacroId,
    name: String,
    #[serde(default)]
    pins: Vec<MacroPin>,
    #[serde(default)]
    package: Package,
    #[serde(default)]
    datasheet: String,
    #[serde(default)]
    doc_link: Option<String>,
    #[serde(default)]
    origin: Option<LibraryRef>,
    /// Informative: recomputed on load.
    #[serde(default)]
    #[allow(dead_code)]
    hash: u64,
    body: CircuitRaw,
}

#[derive(Serialize)]
pub(crate) struct MacroRecordOut<'a> {
    id: MacroId,
    name: &'a str,
    pins: &'a [MacroPin],
    #[serde(skip_serializing_if = "is_auto")]
    package: Package,
    #[serde(skip_serializing_if = "str::is_empty")]
    datasheet: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc_link: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: &'a Option<LibraryRef>,
    hash: u64,
    body: &'a Circuit,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde skip predicate signature")]
fn is_auto(p: &Package) -> bool {
    *p == Package::Auto
}

/// Records of the given definitions of a table (in the order given).
pub(crate) fn macro_records<'a>(t: &'a MacroTable, ids: &[MacroId]) -> Vec<MacroRecordOut<'a>> {
    ids.iter()
        .filter_map(|&id| {
            let e = t.get(id)?;
            let d = &e.def;
            Some(MacroRecordOut {
                id,
                name: &d.name,
                pins: &d.pins,
                package: d.package,
                datasheet: &d.datasheet,
                doc_link: &d.doc_link,
                origin: &d.origin,
                hash: e.hash,
                body: &d.body,
            })
        })
        .collect()
}

/// Builds a definitions table from file records: each definition after the
/// ones it uses; cycles, unknown references, depth and size limits refused.
pub(crate) fn build_table(records: Vec<MacroRecordIn>) -> Result<MacroTable, IoError> {
    if records.len() > MAX_MACROS {
        return Err(IoError::LimitExceeded { what: "macro definitions", limit: MAX_MACROS });
    }
    let mut components = 0usize;
    let mut pending: std::collections::BTreeMap<MacroId, MacroRecordIn> = std::collections::BTreeMap::new();
    for r in records {
        components += r.body.components.len();
        if components > MAX_COMPONENTS {
            return Err(IoError::LimitExceeded { what: "components", limit: MAX_COMPONENTS });
        }
        let id = r.id;
        if pending.insert(id, r).is_some() {
            return Err(IoError::Parse(format!("duplicate macro id {}", id.0)));
        }
    }
    let uses = |r: &MacroRecordIn| -> Vec<MacroId> {
        r.body
            .components
            .iter()
            .filter_map(|c| match c.kind {
                ComponentKind::Macro(s) => Some(s.def),
                _ => None,
            })
            .collect()
    };
    let mut table = MacroTable::default();
    while !pending.is_empty() {
        let ready: Vec<MacroId> = pending
            .iter()
            .filter(|(_, r)| uses(r).iter().all(|d| table.get(*d).is_some() || !pending.contains_key(d)))
            .map(|(&id, _)| id)
            .collect();
        if ready.is_empty() {
            let id = pending.keys().next().map_or(0, |m| m.0);
            return Err(IoError::Parse(format!("macro {id} contains itself (definition cycle)")));
        }
        for id in ready {
            let Some(r) = pending.remove(&id) else { continue };
            let bad = |e: bw_core::CircuitError| IoError::Parse(format!("macro {} ({:?}): {e}", id.0, r.name));
            check_raw_limits(&r.body)?;
            let body =
                Circuit::from_raw(r.body, &table, true).map_err(|e| IoError::Parse(format!("macro {}: {e}", id.0)))?;
            let def = MacroDef {
                name: r.name.clone(),
                body,
                pins: r.pins,
                package: r.package,
                datasheet: r.datasheet,
                doc_link: r.doc_link,
                origin: r.origin,
            };
            table.add(id, Arc::new(def)).map_err(bad)?;
        }
    }
    Ok(table)
}

fn check_raw_limits(raw: &CircuitRaw) -> Result<(), IoError> {
    if raw.components.len() > MAX_COMPONENTS {
        return Err(IoError::LimitExceeded { what: "components", limit: MAX_COMPONENTS });
    }
    if raw.segments.len() > MAX_SEGMENTS {
        return Err(IoError::LimitExceeded { what: "wire segments", limit: MAX_SEGMENTS });
    }
    if raw.nodes.len() > MAX_WIRE_NODES {
        return Err(IoError::LimitExceeded { what: "wire nodes", limit: MAX_WIRE_NODES });
    }
    if raw.memories.len() > MAX_MEMORIES {
        return Err(IoError::LimitExceeded { what: "memories", limit: MAX_MEMORIES });
    }
    Ok(())
}

#[derive(Deserialize)]
struct Header {
    format: String,
    version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIn<C> {
    #[allow(dead_code)]
    format: String,
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    macros: Vec<MacroRecordIn>,
    circuit: C,
}

/// Circuit of a version 1 or 2 file: point-to-point wires.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCircuit {
    components: Vec<Component>,
    wires: Vec<LegacyWire>,
    #[serde(default)]
    memories: Vec<MemoryRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
struct LegacyWireId(u32);

/// A version 1 / 2 wire: from an output pin to an input pin through
/// purely visual vertices.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyWire {
    id: LegacyWireId,
    from: PinRef,
    to: PinRef,
    #[serde(default)]
    via: Vec<GridPos>,
}

fn ron_options() -> ron::Options {
    ron::Options::default().with_recursion_limit(RON_RECURSION_LIMIT)
}

/// Parses a project from untrusted bytes.
///
/// # Errors
/// Any violation of the format, limits or circuit invariants.
pub fn load_from_bytes(bytes: &[u8]) -> Result<Project, IoError> {
    if bytes.len() as u64 > MAX_FILE_SIZE {
        return Err(IoError::TooLarge);
    }
    let opts = ron_options();
    // Pass 1: header only (unknown fields ignored) for a clear version error.
    let header: Header = opts.from_bytes(bytes).map_err(|_| IoError::NotABitwerkFile)?;
    if header.format != FORMAT_MAGIC {
        return Err(IoError::NotABitwerkFile);
    }
    if header.version > FORMAT_VERSION || header.version < OLDEST_VERSION {
        return Err(IoError::UnsupportedVersion { found: header.version });
    }
    // Pass 2: full, strict parse (validates the circuit).
    let project = if header.version >= 3 {
        let file: FileIn<CircuitRaw> = opts.from_bytes(bytes).map_err(|e| IoError::Parse(e.to_string()))?;
        if header.version < 4 {
            let feature = if file.macros.is_empty() { macro_feature(&file.circuit) } else { Some("macros") };
            if let Some(feature) = feature {
                return Err(IoError::FeatureNotInVersion { version: header.version, feature });
            }
        }
        if header.version < 5 {
            let bodies = file.macros.iter().map(|m| &m.body);
            if let Some(feature) = std::iter::once(&file.circuit).chain(bodies).find_map(v5_feature) {
                return Err(IoError::FeatureNotInVersion { version: header.version, feature });
            }
        }
        check_raw_limits(&file.circuit)?;
        let table = build_table(file.macros)?;
        let circuit = Circuit::from_raw(file.circuit, &table, false).map_err(|e| IoError::Parse(e.to_string()))?;
        Project { name: file.name, circuit }
    } else {
        let file: FileIn<LegacyCircuit> = opts.from_bytes(bytes).map_err(|e| IoError::Parse(e.to_string()))?;
        if !file.macros.is_empty() {
            return Err(IoError::FeatureNotInVersion { version: header.version, feature: "macros" });
        }
        let circuit = upgrade_wires(header.version, file.circuit)?;
        migrate(header.version, Project { name: file.name, circuit })?
    };
    check_limits(&project.circuit, &project.name)?;
    Ok(project)
}

/// Format 5 additions used by a circuit (see the version history).
pub(crate) fn v5_feature(c: &CircuitRaw) -> Option<&'static str> {
    if !c.notes.is_empty() {
        return Some("text annotations");
    }
    if c.eval != bw_core::EvalMode::UnitDelay {
        return Some("the sequential simulation mode");
    }
    c.components.iter().find_map(|k| {
        if k.color.is_some() {
            return Some("display colours");
        }
        match k.kind {
            ComponentKind::Const { .. } => Some("power / ground"),
            ComponentKind::SeqGen { .. } => Some("sequence generators"),
            ComponentKind::SevenSeg { .. } => Some("7-segment displays"),
            ComponentKind::Numeric { .. } => Some("numeric outputs"),
            ComponentKind::Text { .. } => Some("text annotations"),
            ComponentKind::Switch { mode, .. } if mode != bw_core::SwitchMode::Toggle => {
                Some("push-button and pulse switches")
            }
            _ => None,
        }
    })
}

/// Format 4 component kinds used by a circuit (macro tags or instances).
fn macro_feature(c: &CircuitRaw) -> Option<&'static str> {
    c.components.iter().find_map(|k| match k.kind {
        ComponentKind::Tag { .. } => Some("macro tags"),
        ComponentKind::Macro(_) => Some("macros"),
        _ => None,
    })
}

/// Version 1 / 2 → 3: every point-to-point wire becomes an orthogonal path
/// (straight or L-shaped; old diagonal vertices get a corner, horizontal
/// first) from its output pin to its input pin. Wires sharing a pin share
/// that pin's node, so fan-out and tri-state buses keep their nets.
fn upgrade_wires(version: u32, legacy: LegacyCircuit) -> Result<Circuit, IoError> {
    if legacy.wires.len() > MAX_LEGACY_WIRES {
        return Err(IoError::LimitExceeded { what: "wires", limit: MAX_LEGACY_WIRES });
    }
    let raw = CircuitRaw { components: legacy.components, memories: legacy.memories, ..CircuitRaw::default() };
    let mut c = Circuit::try_from(raw).map_err(|e| IoError::Parse(e.to_string()))?;
    let mut wires = legacy.wires;
    wires.sort_by_key(|w| w.id);
    let mut seen = std::collections::HashSet::with_capacity(wires.len());
    for (i, w) in wires.iter().enumerate() {
        let bad = |what: &str| IoError::Parse(format!("format {version} wire {}: {what}", w.id.0));
        if i > 0 && wires[i - 1].id == w.id {
            return Err(bad("duplicate id"));
        }
        if w.via.len() > bw_core::MAX_WIRE_VERTICES {
            return Err(bad("too many vertices"));
        }
        let dir = |p: PinRef| c.component(p.component).and_then(|k| k.pin_dir(p.pin));
        if dir(w.from) != Some(PinDir::Output) || dir(w.to) != Some(PinDir::Input) {
            return Err(bad("must go from an output pin to an input pin"));
        }
        if !seen.insert((w.from, w.to)) {
            return Err(bad("duplicates another wire"));
        }
        c.connect(w.from, w.to, w.via.clone()).map_err(|e| bad(&e.to_string()))?;
    }
    Ok(c)
}

/// Brings a project read from a file of format `version` up to
/// [`FORMAT_VERSION`].
///
/// Version 1 → 2: the circuit schema is a strict subset, so the data carries
/// over unchanged; a version 1 file must not use anything version 2 added
/// (that would be a forged or corrupted file). Semantically, version 1
/// treated unconnected inputs as 0; version 2 reads them as floating (Z,
/// which logic sees as X).
///
/// Version 2 → 3 (wires to a wire graph) happens while parsing, see
/// `upgrade_wires`. Version 3 → 4 adds macros and needs no change (a
/// version 3 file using format 4 kinds is refused). Version 4 → 5 adds
/// kinds, colours, notes and the simulation mode, all with defaults (a
/// version 4 file using them is refused while parsing). Saving writes
/// version 5.
///
/// # Errors
/// [`IoError::FeatureNotInVersion`] for a version 1 file with version 2 content.
pub fn migrate(version: u32, project: Project) -> Result<Project, IoError> {
    for c in project.circuit.components() {
        let feature = match c.kind {
            ComponentKind::TriState if version < 2 => "tri-state buffers",
            ComponentKind::FlipFlop { .. } if version < 2 => "flip-flops",
            ComponentKind::Memory { .. } if version < 2 => "memories",
            ComponentKind::Tag { .. } if version < 4 => "macro tags",
            ComponentKind::Macro(_) if version < 4 => "macros",
            _ => continue,
        };
        return Err(IoError::FeatureNotInVersion { version, feature });
    }
    Ok(project)
}

pub(crate) fn check_limits(c: &Circuit, name: &str) -> Result<(), IoError> {
    if c.component_count() > MAX_COMPONENTS {
        return Err(IoError::LimitExceeded { what: "components", limit: MAX_COMPONENTS });
    }
    if c.segment_count() > MAX_SEGMENTS {
        return Err(IoError::LimitExceeded { what: "wire segments", limit: MAX_SEGMENTS });
    }
    if c.node_count() > MAX_WIRE_NODES {
        return Err(IoError::LimitExceeded { what: "wire nodes", limit: MAX_WIRE_NODES });
    }
    if c.junction_count() > MAX_JUNCTIONS {
        return Err(IoError::LimitExceeded { what: "junctions", limit: MAX_JUNCTIONS });
    }
    let memories = c.components().filter(|k| matches!(k.kind, bw_core::ComponentKind::Memory { .. })).count();
    if memories > MAX_MEMORIES {
        return Err(IoError::LimitExceeded { what: "memories", limit: MAX_MEMORIES });
    }
    if name.len() > MAX_NAME_LEN {
        return Err(IoError::LimitExceeded { what: "name bytes", limit: MAX_NAME_LEN });
    }
    if c.macros().len() > MAX_MACROS {
        return Err(IoError::LimitExceeded { what: "macro definitions", limit: MAX_MACROS });
    }
    if c.flat_size() > MAX_FLAT_COMPONENTS {
        return Err(IoError::LimitExceeded {
            what: "components when flattened",
            limit: usize::try_from(MAX_FLAT_COMPONENTS).unwrap_or(usize::MAX),
        });
    }
    Ok(())
}

/// Reads a project from disk. The size limit is enforced while reading, so a
/// file growing after the metadata check cannot bypass it.
///
/// # Errors
/// I/O errors and everything [`load_from_bytes`] rejects.
pub fn load_from_path(path: &Path) -> Result<Project, IoError> {
    let buf = read_capped(path)?;
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("dwm")) {
        return dwm::load(&buf);
    }
    load_from_bytes(&buf)
}

/// Reads a regular file of at most [`MAX_FILE_SIZE`] bytes (the limit is
/// enforced while reading too).
///
/// # Errors
/// Not a regular file, too large, or I/O failure.
pub fn read_capped(path: &Path) -> Result<Vec<u8>, IoError> {
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(IoError::NotABitwerkFile);
    }
    if meta.len() > MAX_FILE_SIZE {
        return Err(IoError::TooLarge);
    }
    let mut buf = Vec::new();
    file.take(MAX_FILE_SIZE + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_FILE_SIZE {
        return Err(IoError::TooLarge);
    }
    Ok(buf)
}

/// Serializes a project to a RON string.
///
/// # Errors
/// Limits exceeded or serializer failure.
pub fn save_to_string(p: &Project) -> Result<String, IoError> {
    check_limits(&p.circuit, &p.name)?;
    let t = p.circuit.macros();
    let ids: Vec<MacroId> = t.iter().map(|(id, _)| id).collect();
    let out = FileOut {
        format: FORMAT_MAGIC,
        version: FORMAT_VERSION,
        name: &p.name,
        macros: macro_records(t, &ids),
        circuit: &p.circuit,
    };
    let cfg = ron::ser::PrettyConfig::new().depth_limit(10);
    ron_options().to_string_pretty(&out, cfg).map_err(|e| IoError::Serialize(e.to_string()))
}

/// Writes a project atomically (temp file in the same directory + rename).
///
/// # Errors
/// I/O or serialization failure.
pub fn save_to_path(p: &Project, path: &Path) -> Result<(), IoError> {
    write_atomic(&save_to_string(p)?, path)
}

/// Writes text atomically (temp file in the same directory + rename).
///
/// # Errors
/// I/O failure.
pub fn write_atomic(text: &str, path: &Path) -> Result<(), IoError> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bw_core::{ComponentKind, Edit, FlipFlopType, GateOp, GridPos, MemoryType, PinRef, Rotation, WireEnd};

    fn sample() -> Project {
        let mut c = Circuit::new();
        let s = c
            .add_component(
                ComponentKind::Switch { on: true, mode: bw_core::SwitchMode::Toggle },
                GridPos::new(0, 0),
                Rotation::R0,
            )
            .unwrap();
        let g = c
            .add_component(ComponentKind::Gate { op: GateOp::Xor, inputs: 2 }, GridPos::new(6, 0), Rotation::R90)
            .unwrap();
        let led = c.add_component(ComponentKind::Led, GridPos::new(0, 8), Rotation::R0).unwrap();
        c.connect(PinRef { component: s, pin: 0 }, PinRef { component: g, pin: 0 }, vec![GridPos::new(4, 1)]).unwrap();
        // A branch off the first wire: a junction.
        let seg = c.segments().next().unwrap().0;
        let (a, _) = c.segment_ends(seg).unwrap();
        c.apply(&Edit::AddWire {
            from: WireEnd::OnSegment(seg, GridPos::new(a.x + 1, a.y)),
            to: WireEnd::Pin(PinRef { component: led, pin: 0 }),
            via: vec![],
        })
        .unwrap();
        assert_eq!(c.junction_count(), 1);
        Project { name: "sample".into(), circuit: c }
    }

    #[test]
    fn roundtrip() {
        let p = sample();
        let text = save_to_string(&p).unwrap();
        assert!(text.contains("version: 5") && text.contains("Free((") && text.contains("segments:"), "{text}");
        let q = load_from_bytes(text.as_bytes()).unwrap();
        assert_eq!(q.name, "sample");
        assert_eq!(q.circuit.component_count(), 3);
        assert_eq!(q.circuit.segment_count(), p.circuit.segment_count());
        assert_eq!(q.circuit.junction_count(), 1);
        assert_eq!(save_to_string(&q).unwrap(), text);
        q.circuit.check().unwrap();
    }

    #[test]
    fn rejects_bad_magic_and_future_version() {
        let p = sample();
        let text = save_to_string(&p).unwrap();
        let wrong = text.replace("\"bitwerk\"", "\"other\"");
        assert!(matches!(load_from_bytes(wrong.as_bytes()), Err(IoError::NotABitwerkFile)));
        let future = text.replace("version: 5", "version: 99");
        assert!(matches!(load_from_bytes(future.as_bytes()), Err(IoError::UnsupportedVersion { found: 99 })));
    }

    fn v3(nodes: &str, segments: &str) -> String {
        format!(
            r#"(format: "bitwerk", version: 3, name: "x", circuit: (
            components: [
                (id: (0), kind: Switch(on: false), pos: (x: 0, y: 0)),
                (id: (1), kind: Led, pos: (x: 6, y: 0)),
                (id: (2), kind: Switch(on: false), pos: (x: 0, y: 6)),
            ],
            nodes: [{nodes}],
            segments: [{segments}],
        ))"#
        )
    }

    #[test]
    fn rejects_invalid_wiring() {
        let pins = "(id: (0), at: Pin((component: (0), pin: 0))), (id: (1), at: Pin((component: (1), pin: 0)))";
        assert!(load_from_bytes(v3(pins, "(a: (0), b: (1))").as_bytes()).is_ok());
        let cases = [
            // Two switches on one net.
            (
                format!(
                    "{pins}, (id: (2), at: Pin((component: (2), pin: 0))), (id: (3), at: Free((x: 4, y: 7))), \
                     (id: (4), at: Free((x: 4, y: 1)))"
                ),
                "(a: (0), b: (1)), (a: (2), b: (3)), (a: (3), b: (4)), (a: (4), b: (0))".to_string(),
                "only tri-state outputs may share a net",
            ),
            // Diagonal segment.
            (
                format!("{pins}, (id: (2), at: Free((x: 3, y: 3)))"),
                "(a: (0), b: (2)), (a: (2), b: (1))".into(),
                "invalid",
            ),
            // Dangling bend.
            (
                format!("{pins}, (id: (2), at: Free((x: 2, y: 5)))"),
                "(a: (0), b: (1)), (a: (0), b: (2))".into(),
                "dangling",
            ),
            // Unknown node, same node twice, duplicate segment, duplicate pin node.
            (pins.into(), "(a: (0), b: (7))".into(), "does not exist"),
            (pins.into(), "(a: (0), b: (0))".into(), "invalid"),
            (pins.into(), "(a: (0), b: (1)), (a: (1), b: (0))".into(), "invalid"),
            (
                format!("{pins}, (id: (2), at: Pin((component: (0), pin: 0)))"),
                "(a: (0), b: (1))".into(),
                "more than one",
            ),
            (pins.into(), "(a: (0), b: (1))".replace("(1))", "(1)), (a: (1), b: (0), c: (2))"), "c"),
        ];
        for (nodes, segs, msg) in cases {
            match load_from_bytes(v3(&nodes, &segs).as_bytes()) {
                Err(IoError::Parse(m)) => assert!(m.contains(msg), "{m} (expected {msg})"),
                other => panic!("{nodes} / {segs}: unexpected {other:?}"),
            }
        }
    }

    /// Two version 2 wires from different outputs into the same input were
    /// already invalid; they still are after the upgrade.
    #[test]
    fn rejects_invalid_legacy_wires() {
        let text = r#"(format: "bitwerk", version: 2, name: "x", circuit: (
            components: [
                (id: (0), kind: Switch(on: false), pos: (x: 0, y: 0)),
                (id: (1), kind: Led, pos: (x: 4, y: 0)),
                (id: (2), kind: Switch(on: false), pos: (x: 0, y: 4)),
            ],
            wires: [
                (id: (0), from: (component: (0), pin: 0), to: (component: (1), pin: 0)),
                (id: (1), from: (component: (2), pin: 0), to: (component: (1), pin: 0)),
            ],
        ))"#;
        match load_from_bytes(text.as_bytes()) {
            Err(IoError::Parse(msg)) => assert!(msg.contains("only tri-state"), "{msg}"),
            other => panic!("unexpected: {other:?}"),
        }
        let backwards = text.replace(
            "(id: (1), from: (component: (2), pin: 0), to: (component: (1), pin: 0)),",
            "(id: (1), from: (component: (1), pin: 0), to: (component: (2), pin: 0)),",
        );
        assert!(matches!(load_from_bytes(backwards.as_bytes()), Err(IoError::Parse(_))));
    }

    /// Every new kind, a tri-state bus and memory contents survive a save /
    /// load round trip, and the contents are stored compactly.
    #[test]
    fn roundtrip_new_components_and_memory() {
        let mut c = Circuit::new();
        let ff = c.add_component(ComponentKind::flip_flop(FlipFlopType::Jk), GridPos::new(0, 0), Rotation::R0).unwrap();
        let t1 = c.add_component(ComponentKind::TriState, GridPos::new(0, 10), Rotation::R0).unwrap();
        let t2 = c.add_component(ComponentKind::TriState, GridPos::new(0, 20), Rotation::R0).unwrap();
        let led = c.add_component(ComponentKind::Led, GridPos::new(10, 10), Rotation::R0).unwrap();
        let rom = ComponentKind::Memory { ty: MemoryType::Rom, addr_bits: 14, data_bits: 16 };
        let m = c.add_component(rom, GridPos::new(20, 0), Rotation::R90).unwrap();
        c.connect(PinRef { component: t1, pin: 2 }, PinRef { component: led, pin: 0 }, vec![]).unwrap();
        c.connect(PinRef { component: t2, pin: 2 }, PinRef { component: led, pin: 0 }, vec![]).unwrap();
        c.connect(PinRef { component: ff, pin: 5 }, PinRef { component: t1, pin: 1 }, vec![]).unwrap();
        let mut words = vec![0u16; 1 << 14];
        words[0] = 0xbeef;
        words[100..200].iter_mut().for_each(|w| *w = 7);
        words[16_383] = 1;
        c.set_memory(m, &words).unwrap();
        let p = Project { name: "new".into(), circuit: c };
        let text = save_to_string(&p).unwrap();
        assert!(
            text.len() < 4_000 && text.contains("beef 0*99 7*100 0*16183 1"),
            "compact memory encoding, got {} bytes",
            text.len()
        );
        let q = load_from_bytes(text.as_bytes()).unwrap();
        assert_eq!(*q.circuit.memory(m).unwrap(), words);
        assert_eq!(q.circuit.drivers_of(PinRef { component: led, pin: 0 }).len(), 2);
        assert_eq!(q.circuit.component(ff).unwrap().kind, ComponentKind::flip_flop(FlipFlopType::Jk));
        assert_eq!(save_to_string(&q).unwrap(), text);
    }

    #[test]
    fn rejects_bad_memory_contents() {
        let base = |kind: &str, words: &str| {
            format!(
                r#"(format: "bitwerk", version: 3, name: "x", circuit: (
                components: [(id: (0), kind: {kind}, pos: (x: 0, y: 0))],
                memories: [(id: (0), words: "{words}")],
            ))"#
            )
        };
        let rom = "Memory(ty: Rom, addr_bits: 4, data_bits: 4)";
        assert!(load_from_bytes(base(rom, "f 1*15").as_bytes()).is_ok());
        for (kind, words) in [
            (rom, "10"),                                                   // wider than 4 bits
            (rom, "1*17"),                                                 // more words than 16
            (rom, "zz"),                                                   // not hex
            ("Led", "1"),                                                  // not a memory
            ("Memory(ty: Rom, addr_bits: 15, data_bits: 4)", ""),          // too many address lines
            ("Memory(ty: Rom, addr_bits: 8, data_bits: 3)", ""),           // unsupported width
            ("FlipFlop(ty: Jk, trigger: Latch, preset_clear: false)", ""), // invalid variant
        ] {
            assert!(load_from_bytes(base(kind, words).as_bytes()).is_err(), "{kind} {words:?} accepted");
        }
    }

    #[test]
    fn rejects_garbage_and_deep_nesting() {
        assert!(load_from_bytes(b"\xff\xfe garbage").is_err());
        let deep = "[".repeat(10_000);
        assert!(load_from_bytes(deep.as_bytes()).is_err());
    }

    #[test]
    fn dwm_garbage_is_a_clean_error() {
        assert!(matches!(dwm::load(b"anything"), Err(IoError::Dwm(_))));
    }
}

#[cfg(test)]
mod example_files {
    #[test]
    fn bundled_examples_load() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut n = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "bitwerk") {
                let p = crate::load_from_path(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(p.circuit.component_count() > 0);
                p.circuit.check().unwrap();
                n += 1;
            }
        }
        assert!(n > 0);
    }
}

#[cfg(test)]
mod migration {
    use std::collections::{BTreeSet, HashMap};

    use bw_core::{Circuit, PinRef};

    /// Nets of a circuit as a set of pin sets.
    fn partition(c: &Circuit) -> BTreeSet<Vec<PinRef>> {
        let nets = c.nets();
        (0..nets.len())
            .map(|i| {
                let mut v = nets.net(i).to_vec();
                v.sort_unstable();
                v
            })
            .collect()
    }

    fn find(p: &mut HashMap<PinRef, PinRef>, x: PinRef) -> PinRef {
        let up = *p.entry(x).or_insert(x);
        if up == x {
            return x;
        }
        let r = find(p, up);
        p.insert(x, r);
        r
    }

    /// The nets the legacy point-to-point wires describe (union-find).
    fn legacy_partition(text: &str) -> BTreeSet<Vec<PinRef>> {
        let file: crate::FileIn<crate::LegacyCircuit> = crate::ron_options().from_str(text).unwrap();
        let mut parent: HashMap<PinRef, PinRef> = HashMap::new();
        for w in &file.circuit.wires {
            let (a, b) = (find(&mut parent, w.from), find(&mut parent, w.to));
            parent.insert(a.max(b), a.min(b));
        }
        let pins: Vec<PinRef> = parent.keys().copied().collect();
        let mut groups: HashMap<PinRef, Vec<PinRef>> = HashMap::new();
        for p in pins {
            let r = find(&mut parent, p);
            groups.entry(r).or_default().push(p);
        }
        groups
            .into_values()
            .map(|mut v| {
                v.sort_unstable();
                v
            })
            .collect()
    }

    /// Files written in formats 1 and 2 (fixtures saved before each format
    /// change) load with every component and position, every old wire turns
    /// into an orthogonal path with exactly the same connectivity, and they
    /// save as format 3 (which loads back identically).
    #[test]
    fn v1_and_v2_fixtures_migrate_to_current_version() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let (mut v1, mut v2) = (0, 0);
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "bitwerk") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            if text.contains("version: 3,") || text.contains("version: 4,") {
                continue; // see v3_fixtures_migrate_to_v4 / v4_fixtures_migrate_to_v5
            }
            if text.contains("version: 1,") {
                v1 += 1;
            } else {
                assert!(text.contains("version: 2,"), "{} is not a v1/v2 fixture", path.display());
                v2 += 1;
            }
            let p = crate::load_from_path(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            p.circuit.check().unwrap();
            assert_eq!(p.circuit.component_count(), text.matches("kind:").count());
            assert_eq!(partition(&p.circuit), legacy_partition(&text), "{}: nets changed", path.display());
            // Every wire end is a pin node exactly on its pin.
            for (_, n) in p.circuit.nodes() {
                if let Some(pin) = n.pin {
                    assert_eq!(p.circuit.component(pin.component).unwrap().pin_pos(pin.pin), Some(n.pos));
                }
            }
            let saved = crate::save_to_string(&p).unwrap();
            assert!(saved.contains(&format!("version: {}", crate::FORMAT_VERSION)));
            let again = crate::load_from_bytes(saved.as_bytes()).unwrap();
            assert_eq!(again.circuit.components().collect::<Vec<_>>(), p.circuit.components().collect::<Vec<_>>());
            assert_eq!(crate::save_to_string(&again).unwrap(), saved);
            assert_eq!(partition(&again.circuit), partition(&p.circuit));
        }
        assert!(v1 >= 2 && v2 >= 4, "fixtures: {v1} v1, {v2} v2");
    }

    /// A version 2 bus (two tri-state outputs wired to the same input) is
    /// one net after the upgrade, with a junction where the paths meet.
    #[test]
    fn v2_bus_becomes_one_net() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let p = crate::load_from_path(&dir.join("v2-flipflops-memory.bitwerk")).unwrap();
        let tri: Vec<_> = p
            .circuit
            .components()
            .filter(|c| c.kind == bw_core::ComponentKind::TriState)
            .map(|c| PinRef { component: c.id, pin: 2 })
            .collect();
        assert!(tri.len() >= 2);
        let drivers = p.circuit.drivers_of(tri[0]);
        assert!(tri.iter().all(|t| drivers.contains(t)), "{drivers:?}");
        assert!(p.circuit.junction_count() > 0);
    }

    /// A file claiming format 1 that uses format 2 components is rejected.
    #[test]
    fn v1_file_with_v2_content_is_rejected() {
        let text = r#"(format: "bitwerk", version: 1, name: "x", circuit: (
            components: [(id: (0), kind: TriState, pos: (x: 0, y: 0))], wires: []))"#;
        assert!(matches!(
            crate::load_from_bytes(text.as_bytes()),
            Err(crate::IoError::FeatureNotInVersion { version: 1, .. })
        ));
    }

    /// Old wires are not accepted in a format 3 file, nor new wiring in an
    /// old one.
    #[test]
    fn formats_do_not_mix() {
        let v3_with_wires = r#"(format: "bitwerk", version: 3, name: "x", circuit: (
            components: [], wires: []))"#;
        assert!(crate::load_from_bytes(v3_with_wires.as_bytes()).is_err());
        let v2_with_nodes = r#"(format: "bitwerk", version: 2, name: "x", circuit: (
            components: [], wires: [], nodes: []))"#;
        assert!(crate::load_from_bytes(v2_with_nodes.as_bytes()).is_err());
    }
}

/// A stable-toolchain stand-in for the fuzz target on the new wiring
/// format: thousands of deterministic mutations of the numbers in valid
/// format 3 files (node ids, coordinates, pin numbers) must never panic,
/// and whatever is accepted satisfies every wiring invariant and
/// round-trips.
#[cfg(test)]
mod mutation {
    #[test]
    fn mutated_wiring_never_panics_and_accepted_files_are_valid() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut seed = 0x853c_49e6_748f_ea9bu64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n.max(1)
        };
        let (mut accepted, mut total) = (0, 0);
        for name in ["half-adder", "bus-contention", "all-components"] {
            let text = std::fs::read_to_string(dir.join(format!("{name}.bitwerk"))).unwrap();
            let start = text.find("nodes:").unwrap();
            // Byte offsets of the numbers in the wiring part.
            let nums: Vec<(usize, usize)> = text[start..]
                .char_indices()
                .filter(|&(i, ch)| {
                    ch.is_ascii_digit() && !text.as_bytes()[start..][i.saturating_sub(1)].is_ascii_digit()
                })
                .map(|(i, _)| {
                    let len = text[start + i..].bytes().take_while(u8::is_ascii_digit).count();
                    (start + i, len)
                })
                .collect();
            for _ in 0..700 {
                let mut t = text.clone();
                for _ in 0..=rnd(3) {
                    let (at, len) = nums[usize::try_from(rnd(nums.len() as u64)).unwrap()];
                    let v = match rnd(4) {
                        0 => rnd(4),
                        1 => rnd(64),
                        2 => u64::from(u32::MAX) - rnd(3),
                        _ => rnd(1_000_000),
                    };
                    if at + len <= t.len() && t.is_char_boundary(at) && t.is_char_boundary(at + len) {
                        t.replace_range(at..at + len, &v.to_string());
                    }
                }
                total += 1;
                if let Ok(p) = crate::load_from_bytes(t.as_bytes()) {
                    accepted += 1;
                    p.circuit.check().unwrap();
                    let again = crate::load_from_bytes(crate::save_to_string(&p).unwrap().as_bytes()).unwrap();
                    assert_eq!(again.circuit.segment_count(), p.circuit.segment_count());
                }
            }
        }
        assert!(accepted > 10 && accepted < total, "{accepted} of {total} mutants accepted");
    }
}

/// Format 4 (macros): migration from format 3 and macro round trips.
#[cfg(test)]
mod v4 {
    use bw_core::{Circuit, ComponentKind, Edit, GridPos, PinRef, Rotation, WireEnd};

    use crate::bwmacro;

    fn nets(c: &Circuit) -> std::collections::BTreeSet<Vec<PinRef>> {
        let n = c.nets();
        (0..n.len())
            .map(|i| {
                let mut v = n.net(i).to_vec();
                v.sort_unstable();
                v
            })
            .collect()
    }

    /// The format 3 fixtures (saved before format 4 existed) load, keep
    /// every component and net, save as format 4 and load back identically.
    #[test]
    fn v3_fixtures_migrate_to_v4() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut n = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "bitwerk") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            if !text.contains("version: 3,") {
                continue;
            }
            n += 1;
            let p = crate::load_from_path(&path).unwrap();
            p.circuit.check().unwrap();
            assert_eq!(p.circuit.component_count(), text.matches("kind:").count());
            let saved = crate::save_to_string(&p).unwrap();
            assert!(saved.contains("version: 5") && !saved.contains("macros:"), "no macro table without macros");
            let q = crate::load_from_bytes(saved.as_bytes()).unwrap();
            assert_eq!(q.circuit.components().collect::<Vec<_>>(), p.circuit.components().collect::<Vec<_>>());
            assert_eq!(nets(&q.circuit), nets(&p.circuit));
            assert_eq!(crate::save_to_string(&q).unwrap(), saved);
        }
        assert_eq!(n, 4, "four format 3 fixtures");
    }

    fn library_part(key: &str) -> &'static crate::library::LibEntry {
        crate::library::builtin().iter().find(|e| e.key == key).unwrap()
    }

    fn project_with_macros() -> crate::Project {
        let src = library_part("builtin/adder-4");
        let mut c = Circuit::new();
        let (defs, root) = c.macros().import_plan(&src.file.table, src.file.root).unwrap();
        for (id, def) in defs {
            c.apply(&Edit::DefineMacro { id, def }).unwrap();
        }
        let shape = c.macros().get(root).unwrap().shape;
        let inst = c.add_component(ComponentKind::Macro(shape), GridPos::new(20, 0), Rotation::R0).unwrap();
        let s = c
            .add_component(
                ComponentKind::Switch { on: true, mode: bw_core::SwitchMode::Toggle },
                GridPos::new(0, 0),
                Rotation::R0,
            )
            .unwrap();
        c.apply(&Edit::AddWire {
            from: WireEnd::Pin(PinRef { component: s, pin: 0 }),
            to: WireEnd::Pin(PinRef { component: inst, pin: 3 }),
            via: vec![],
        })
        .unwrap();
        crate::Project { name: "adder".into(), circuit: c }
    }

    #[test]
    fn project_with_macros_round_trips() {
        let p = project_with_macros();
        assert_eq!(p.circuit.macros().len(), 3, "adder-4, full adder, half adder");
        let text = crate::save_to_string(&p).unwrap();
        assert!(text.contains("macros:") && text.contains("kind: Macro(("), "{}", &text[..400]);
        let q = crate::load_from_bytes(text.as_bytes()).unwrap();
        assert_eq!(crate::save_to_string(&q).unwrap(), text);
        for (id, e) in p.circuit.macros().iter() {
            assert_eq!(q.circuit.macros().get(id).unwrap().hash, e.hash, "content hash is stable");
        }
        q.circuit.check().unwrap();
        // A format 3 file may not use macros.
        let old = text.replace("version: 5", "version: 3");
        assert!(matches!(crate::load_from_bytes(old.as_bytes()), Err(crate::IoError::FeatureNotInVersion { .. })));
    }

    #[test]
    fn bwmacro_round_trip_and_malformed_files() {
        let src = library_part("builtin/adder-4");
        let text = bwmacro::save_to_string(&src.file.table, src.file.root).unwrap();
        let f = bwmacro::load_from_bytes(text.as_bytes()).unwrap();
        assert_eq!(f.hash(), src.file.hash());
        assert_eq!(bwmacro::save_to_string(&f.table, f.root).unwrap(), text);
        let bad = |t: &str| bwmacro::load_from_bytes(t.as_bytes()).is_err();
        assert!(bad(&text.replace("bitwerk-macro", "bitwerk")), "magic");
        assert!(bad(&text.replace("version: 2,", "version: 3,")), "version");
        assert!(!bad(&text.replace("version: 2,", "version: 1,")), "version 1 still loads");
        assert!(bad(&text.replace("root: (", "root: (9")), "unknown root");
        assert!(bad("garbage"));
        assert!(bad(&"(".repeat(5000)), "deep nesting");
        // A definition referring to an unknown one, or to itself.
        let first_ref = text.find("kind: Macro((").unwrap() + "kind: Macro((".len();
        let mut t = text.clone();
        t.replace_range(first_ref..=first_ref, "7");
        assert!(bad(&t), "unknown nested definition");
        // Self reference: the half adder record (id 0) instantiating itself.
        let cyc = text.replacen("components: [", "components: [(id: (900), kind: Macro((0)), pos: (x: 0, y: 99)),", 1);
        assert!(bad(&cyc), "cycle");
        // Mutations never panic; anything accepted is valid.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut accepted = 0;
        for _ in 0..400 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut b = text.clone().into_bytes();
            let i = usize::try_from(seed % b.len() as u64).unwrap();
            b[i] = b"0123456789(),:[]x"[usize::try_from(seed >> 40).unwrap() % 17];
            if let Ok(f) = bwmacro::load_from_bytes(&b) {
                accepted += 1;
                for (_, e) in f.table.iter() {
                    e.def.body.check().unwrap();
                }
            }
        }
        assert!(accepted < 400);
    }
}

/// Format 5: new kinds, colours, notes and the simulation mode.
#[cfg(test)]
mod v5 {
    use bw_core::{
        Circuit, Color, ComponentKind, Edit, EvalMode, GridPos, Note, NumFormat, PinRef, Rotation, SwitchMode,
    };

    fn bitwerk_fixtures(version: &str) -> Vec<(std::path::PathBuf, String)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut v: Vec<(std::path::PathBuf, String)> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let p = e.ok()?.path();
                if p.extension().is_none_or(|x| x != "bitwerk") {
                    return None;
                }
                let t = std::fs::read_to_string(&p).ok()?;
                t.contains(&format!("version: {version},")).then_some((p, t))
            })
            .collect();
        v.sort();
        v
    }

    /// The format 4 fixtures (saved by the previous version, before format
    /// 5 existed) load with the same components, nets and macros, save as
    /// format 5 and load back identically.
    #[test]
    fn v4_fixtures_migrate_to_v5() {
        let files = bitwerk_fixtures("4");
        assert_eq!(files.len(), 5, "five format 4 fixtures");
        for (path, _) in files {
            let p = crate::load_from_path(&path).unwrap();
            p.circuit.check().unwrap();
            let saved = crate::save_to_string(&p).unwrap();
            assert!(saved.contains("version: 5,"), "{}", path.display());
            let q = crate::load_from_bytes(saved.as_bytes()).unwrap();
            assert!(q.circuit.content_eq(&p.circuit), "{}", path.display());
            assert_eq!(q.circuit.macros().len(), p.circuit.macros().len());
            for (id, e) in p.circuit.macros().iter() {
                assert_eq!(q.circuit.macros().get(id).map(|x| x.hash), Some(e.hash), "macro hashes are stable");
            }
            assert_eq!(crate::save_to_string(&q).unwrap(), saved);
        }
    }

    fn project() -> crate::Project {
        let mut c = Circuit::new();
        let add = |c: &mut Circuit, kind, x| c.add_component(kind, GridPos::new(x, 0), Rotation::R0).unwrap();
        let vcc = add(&mut c, ComponentKind::Const { high: true }, 0);
        add(&mut c, ComponentKind::Const { high: false }, 4);
        add(&mut c, ComponentKind::SeqGen { pattern: 0b1011, len: 4, half_period: 3 }, 8);
        let seg = add(&mut c, ComponentKind::SevenSeg { common_anode: true }, 14);
        add(&mut c, ComponentKind::Numeric { bits: 8, format: NumFormat::Dec, signed: true }, 24);
        add(&mut c, ComponentKind::Switch { on: true, mode: SwitchMode::Momentary }, 40);
        add(&mut c, ComponentKind::Switch { on: false, mode: SwitchMode::Pulse }, 44);
        let led = add(&mut c, ComponentKind::Led, 48);
        let text = add(&mut c, ComponentKind::Text { w: 1, h: 1 }, 52);
        c.apply(&Edit::SetColor(led, Some(Color::Blue))).unwrap();
        c.apply(&Edit::SetColor(seg, Some(Color::Lime))).unwrap();
        let note = Note { text: "Hello\nworld Ā".into(), height: 20, bold: true, italic: true, overbar: true };
        c.apply(&Edit::SetNote { id: text, note: note.clone() }).unwrap();
        c.apply(&Edit::SetEvalMode(EvalMode::Sequential)).unwrap();
        c.connect(PinRef { component: vcc, pin: 0 }, PinRef { component: seg, pin: 0 }, vec![]).unwrap();
        assert_eq!(c.component(text).unwrap().kind, note.kind(), "the box follows the note");
        crate::Project { name: "v5".into(), circuit: c }
    }

    #[test]
    fn new_features_round_trip() {
        let p = project();
        let text = crate::save_to_string(&p).unwrap();
        let q = crate::load_from_bytes(text.as_bytes()).unwrap();
        assert!(q.circuit.content_eq(&p.circuit));
        assert_eq!(q.circuit.eval_mode(), EvalMode::Sequential);
        assert_eq!(crate::save_to_string(&q).unwrap(), text);
        // The same file claiming to be format 4 is refused.
        let old = text.replacen("version: 5,", "version: 4,", 1);
        assert!(matches!(crate::load_from_bytes(old.as_bytes()), Err(crate::IoError::FeatureNotInVersion { .. })));
    }

    #[test]
    fn bad_notes_and_kinds_are_refused() {
        let text = crate::save_to_string(&project()).unwrap();
        for (from, to) in [
            ("height: 20", "height: 0"),
            ("len: 4", "len: 0"),
            ("len: 4", "len: 65"),
            ("bits: 8", "bits: 1"),
            ("bits: 8", "bits: 17"),
            ("\"Hello", "\"\\u{7}Hello"),
        ] {
            assert!(text.contains(from), "{from}");
            let t = text.replacen(from, to, 1);
            assert!(crate::load_from_bytes(t.as_bytes()).is_err(), "{from} -> {to} accepted");
        }
    }
}
