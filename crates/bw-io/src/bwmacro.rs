//! `.bwmacro`: one macro definition with every definition it uses
//! embedded, so the file is self-contained.
//!
//! ```ron
//! (
//!     format: "bitwerk-macro",
//!     version: 2,
//!     root: (2),
//!     macros: [ (id: (0), name: "Half adder", ...), ..., (id: (2), ...) ],
//! )
//! ```
//!
//! The records are those of the project format 5 (version 1 files: format 4). Loading treats the file
//! as hostile input exactly like a project (size cap, RON depth limit,
//! object limits, full validation, cycles / depth / flattened size refused).

use std::path::Path;

use bw_core::{MacroDef, MacroId, MacroTable};
use serde::{Deserialize, Serialize};

use crate::{IoError, MAX_FILE_SIZE, MacroRecordIn, MacroRecordOut, build_table, macro_records, ron_options};

/// Magic string of a macro file.
pub const MACRO_MAGIC: &str = "bitwerk-macro";
/// Current macro file version: 2 = the records of project format 5 (new
/// kinds, colours, notes); version 1 files (format 4 records) still load.
pub const MACRO_VERSION: u32 = 2;
/// File extension.
pub const MACRO_EXT: &str = "bwmacro";

/// A loaded macro file: its own small definitions table and the
/// definition the file is about.
#[derive(Debug, Clone)]
pub struct MacroFile {
    pub table: MacroTable,
    pub root: MacroId,
}

impl MacroFile {
    /// The main definition.
    #[must_use]
    pub fn def(&self) -> Option<&MacroDef> {
        self.table.def(self.root).map(AsRef::as_ref)
    }

    /// Content hash of the main definition.
    #[must_use]
    pub fn hash(&self) -> u64 {
        self.table.get(self.root).map_or(0, |e| e.hash)
    }
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
    root: MacroId,
    macros: Vec<MacroRecordIn>,
}

#[derive(Serialize)]
struct FileOut<'a> {
    format: &'a str,
    version: u32,
    root: MacroId,
    macros: Vec<MacroRecordOut<'a>>,
}

/// Parses a macro file from untrusted bytes.
///
/// # Errors
/// Any violation of the format, limits or circuit invariants.
pub fn load_from_bytes(bytes: &[u8]) -> Result<MacroFile, IoError> {
    if bytes.len() as u64 > MAX_FILE_SIZE {
        return Err(IoError::TooLarge);
    }
    let opts = ron_options();
    let h: Header = opts.from_bytes(bytes).map_err(|_| IoError::NotABitwerkFile)?;
    if h.format != MACRO_MAGIC {
        return Err(IoError::NotABitwerkFile);
    }
    if !(1..=MACRO_VERSION).contains(&h.version) {
        return Err(IoError::UnsupportedVersion { found: h.version });
    }
    let file: FileIn = opts.from_bytes(bytes).map_err(|e| IoError::Parse(e.to_string()))?;
    if h.version < 2 {
        if let Some(feature) = file.macros.iter().find_map(|m| crate::v5_feature(&m.body)) {
            return Err(IoError::FeatureNotInVersion { version: 1, feature });
        }
    }
    let root = file.root;
    let table = build_table(file.macros)?;
    if table.get(root).is_none() {
        return Err(IoError::Parse(format!("macro file root {} is not defined", root.0)));
    }
    // Everything in the file must be needed by the root.
    let needed = table.closure(root);
    if needed.len() != table.len() {
        return Err(IoError::Parse("macro file contains definitions its root does not use".into()));
    }
    Ok(MacroFile { table, root })
}

/// Reads a macro file from disk (size capped while reading).
///
/// # Errors
/// I/O errors and everything [`load_from_bytes`] rejects.
pub fn load_from_path(path: &Path) -> Result<MacroFile, IoError> {
    load_from_bytes(&crate::read_capped(path)?)
}

/// Serializes definition `root` of `table` with everything it uses.
///
/// # Errors
/// Unknown root or serializer failure.
pub fn save_to_string(table: &MacroTable, root: MacroId) -> Result<String, IoError> {
    if table.get(root).is_none() {
        return Err(IoError::Serialize(format!("macro {} does not exist", root.0)));
    }
    let ids = table.closure(root);
    let out = FileOut { format: MACRO_MAGIC, version: MACRO_VERSION, root, macros: macro_records(table, &ids) };
    let cfg = ron::ser::PrettyConfig::new().depth_limit(10);
    ron_options().to_string_pretty(&out, cfg).map_err(|e| IoError::Serialize(e.to_string()))
}

/// Writes a macro file atomically.
///
/// # Errors
/// I/O or serialization failure.
pub fn save_to_path(table: &MacroTable, root: MacroId, path: &Path) -> Result<(), IoError> {
    crate::write_atomic(&save_to_string(table, root)?, path)
}
