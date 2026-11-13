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
//!
//! Loading treats every file as hostile input: the size is capped *before*
//! reading, RON nesting depth is bounded, object counts are bounded, and the
//! circuit is fully validated (see [`bw_core::Circuit`]) before it is returned.
//! Nothing in a file is ever executed.

#![forbid(unsafe_code)]

pub mod dwm;
pub mod memmap;

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use bw_core::{Circuit, CircuitRaw, Component, GridPos, MemoryRecord, PinDir, PinRef};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Magic string identifying a bitwerk project.
pub const FORMAT_MAGIC: &str = "bitwerk";
/// Current (and highest understood) format version.
pub const FORMAT_VERSION: u32 = 3;
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
/// Maximum macro nesting depth (macros are not implemented yet; the limit is
/// part of the format contract so future versions must honour it).
pub const MAX_MACRO_DEPTH: usize = 16;
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
    circuit: &'a Circuit,
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
        let file: FileIn<Circuit> = opts.from_bytes(bytes).map_err(|e| IoError::Parse(e.to_string()))?;
        Project { name: file.name, circuit: file.circuit }
    } else {
        let file: FileIn<LegacyCircuit> = opts.from_bytes(bytes).map_err(|e| IoError::Parse(e.to_string()))?;
        let circuit = upgrade_wires(header.version, file.circuit)?;
        migrate(header.version, Project { name: file.name, circuit })?
    };
    check_limits(&project.circuit, &project.name)?;
    Ok(project)
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
/// `upgrade_wires`. Saving writes version 3.
///
/// # Errors
/// [`IoError::FeatureNotInVersion`] for a version 1 file with version 2 content.
pub fn migrate(version: u32, project: Project) -> Result<Project, IoError> {
    if version < 2 {
        for c in project.circuit.components() {
            let feature = match c.kind {
                bw_core::ComponentKind::TriState => "tri-state buffers",
                bw_core::ComponentKind::FlipFlop { .. } => "flip-flops",
                bw_core::ComponentKind::Memory { .. } => "memories",
                _ => continue,
            };
            return Err(IoError::FeatureNotInVersion { version, feature });
        }
    }
    Ok(project)
}

fn check_limits(c: &Circuit, name: &str) -> Result<(), IoError> {
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
    Ok(())
}

/// Reads a project from disk. The size limit is enforced while reading, so a
/// file growing after the metadata check cannot bypass it.
///
/// # Errors
/// I/O errors and everything [`load_from_bytes`] rejects.
pub fn load_from_path(path: &Path) -> Result<Project, IoError> {
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
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("dwm")) {
        return dwm::load(&buf);
    }
    load_from_bytes(&buf)
}

/// Serializes a project to a RON string.
///
/// # Errors
/// Limits exceeded or serializer failure.
pub fn save_to_string(p: &Project) -> Result<String, IoError> {
    check_limits(&p.circuit, &p.name)?;
    let out = FileOut { format: FORMAT_MAGIC, version: FORMAT_VERSION, name: &p.name, circuit: &p.circuit };
    let cfg = ron::ser::PrettyConfig::new().depth_limit(8);
    ron_options().to_string_pretty(&out, cfg).map_err(|e| IoError::Serialize(e.to_string()))
}

/// Writes a project atomically (temp file in the same directory + rename).
///
/// # Errors
/// I/O or serialization failure.
pub fn save_to_path(p: &Project, path: &Path) -> Result<(), IoError> {
    let text = save_to_string(p)?;
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
        let s = c.add_component(ComponentKind::Switch { on: true }, GridPos::new(0, 0), Rotation::R0).unwrap();
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
        assert!(text.contains("version: 3") && text.contains("Free((") && text.contains("segments:"), "{text}");
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
        let future = text.replace("version: 3", "version: 99");
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
    fn dwm_is_not_supported_yet() {
        assert!(matches!(dwm::load(b"anything"), Err(IoError::LegacyNotSupported)));
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
            let text = std::fs::read_to_string(&path).unwrap();
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
