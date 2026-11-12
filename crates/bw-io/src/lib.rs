//! `bw-io` — bitwerk project file format.
//!
//! Files are RON documents with a versioned header:
//!
//! ```ron
//! (
//!     format: "bitwerk",
//!     version: 1,
//!     name: "half adder",
//!     circuit: (components: [...], wires: [...]),
//! )
//! ```
//!
//! Loading treats every file as hostile input: the size is capped *before*
//! reading, RON nesting depth is bounded, object counts are bounded, and the
//! circuit is fully validated (see [`bw_core::Circuit`]) before it is returned.
//! Nothing in a file is ever executed.

#![forbid(unsafe_code)]

pub mod dwm;

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use bw_core::Circuit;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Magic string identifying a bitwerk project.
pub const FORMAT_MAGIC: &str = "bitwerk";
/// Current (and highest understood) format version.
pub const FORMAT_VERSION: u32 = 1;
/// Maximum accepted file size in bytes.
pub const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;
/// Maximum number of components in one project.
pub const MAX_COMPONENTS: usize = 1_000_000;
/// Maximum number of wires in one project.
pub const MAX_WIRES: usize = 2_000_000;
/// Maximum macro nesting depth (macros are not implemented yet; the limit is
/// part of the format contract so future versions must honour it).
pub const MAX_MACRO_DEPTH: usize = 16;
/// Maximum project name length in bytes.
pub const MAX_NAME_LEN: usize = 256;
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
struct FileIn {
    #[allow(dead_code)]
    format: String,
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    name: String,
    circuit: Circuit,
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
    if header.version > FORMAT_VERSION || header.version == 0 {
        return Err(IoError::UnsupportedVersion { found: header.version });
    }
    // Pass 2: full, strict parse (validates the circuit).
    let file: FileIn = opts.from_bytes(bytes).map_err(|e| IoError::Parse(e.to_string()))?;
    check_limits(&file.circuit, &file.name)?;
    Ok(Project { name: file.name, circuit: file.circuit })
}

fn check_limits(c: &Circuit, name: &str) -> Result<(), IoError> {
    if c.component_count() > MAX_COMPONENTS {
        return Err(IoError::LimitExceeded { what: "components", limit: MAX_COMPONENTS });
    }
    if c.wire_count() > MAX_WIRES {
        return Err(IoError::LimitExceeded { what: "wires", limit: MAX_WIRES });
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
    use bw_core::{ComponentKind, GateOp, GridPos, PinRef, Rotation};

    fn sample() -> Project {
        let mut c = Circuit::new();
        let s = c.add_component(ComponentKind::Switch { on: true }, GridPos::new(0, 0), Rotation::R0).unwrap();
        let g = c
            .add_component(ComponentKind::Gate { op: GateOp::Xor, inputs: 2 }, GridPos::new(4, 0), Rotation::R90)
            .unwrap();
        c.connect(PinRef { component: s, pin: 0 }, PinRef { component: g, pin: 0 }, vec![GridPos::new(2, 1)]).unwrap();
        Project { name: "sample".into(), circuit: c }
    }

    #[test]
    fn roundtrip() {
        let p = sample();
        let text = save_to_string(&p).unwrap();
        let q = load_from_bytes(text.as_bytes()).unwrap();
        assert_eq!(q.name, "sample");
        assert_eq!(q.circuit.component_count(), 2);
        assert_eq!(q.circuit.wire_count(), 1);
        assert_eq!(save_to_string(&q).unwrap(), text);
    }

    #[test]
    fn rejects_bad_magic_and_future_version() {
        let p = sample();
        let text = save_to_string(&p).unwrap();
        let wrong = text.replace("\"bitwerk\"", "\"other\"");
        assert!(matches!(load_from_bytes(wrong.as_bytes()), Err(IoError::NotABitwerkFile)));
        let future = text.replace("version: 1", "version: 99");
        assert!(matches!(load_from_bytes(future.as_bytes()), Err(IoError::UnsupportedVersion { found: 99 })));
    }

    #[test]
    fn rejects_invalid_circuit() {
        // Two wires driving the same input.
        let text = r#"(format: "bitwerk", version: 1, name: "x", circuit: (
            components: [
                (id: (0), kind: Switch(on: false), pos: (x: 0, y: 0)),
                (id: (1), kind: Led, pos: (x: 4, y: 0)),
            ],
            wires: [
                (id: (0), from: (component: (0), pin: 0), to: (component: (1), pin: 0)),
                (id: (1), from: (component: (0), pin: 0), to: (component: (1), pin: 0)),
            ],
        ))"#;
        match load_from_bytes(text.as_bytes()) {
            Err(IoError::Parse(msg)) => assert!(msg.contains("already driven"), "{msg}"),
            other => panic!("unexpected: {other:?}"),
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
                n += 1;
            }
        }
        assert!(n > 0);
    }
}
