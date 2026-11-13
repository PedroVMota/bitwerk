//! The macro library behind the Parts Centre.
//!
//! * **Built-in parts** are `.bwmacro` files compiled into the binary
//!   (`include_str!` of `crates/bw-io/library/*.bwmacro`). They are original
//!   circuits built from bitwerk's primitives by a reproducible generator
//!   (`crates/bw-sim/tests/library.rs`, which also tests the behaviour of
//!   every part and checks that the embedded files are up to date).
//! * **User parts** live as `.bwmacro` files in a folder (by default
//!   `~/Library/Application Support/bitwerk/Parts`, created on demand), one
//!   sub-folder level per library folder. Scanning never follows symbolic
//!   links, skips hidden entries and is bounded in depth and file count;
//!   every file is parsed as hostile input.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use bw_core::{MacroId, MacroTable};

use crate::IoError;
use crate::bwmacro::{self, MACRO_EXT, MacroFile};

/// A built-in part: library key, folder and the embedded file.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinSource {
    pub key: &'static str,
    pub folder: &'static str,
    pub text: &'static str,
}

macro_rules! builtin {
    ($key:literal, $folder:literal, $file:literal) => {
        BuiltinSource { key: $key, folder: $folder, text: include_str!(concat!("../library/", $file, ".bwmacro")) }
    };
}

/// Every built-in part.
pub const BUILTIN: &[BuiltinSource] = &[
    builtin!("builtin/half-adder", "Arithmetic", "half-adder"),
    builtin!("builtin/full-adder", "Arithmetic", "full-adder"),
    builtin!("builtin/adder-4", "Arithmetic", "adder-4"),
    builtin!("builtin/mux-2", "Selectors", "mux-2"),
    builtin!("builtin/mux-4", "Selectors", "mux-4"),
    builtin!("builtin/decoder-3to8", "Selectors", "decoder-3to8"),
    builtin!("builtin/register-4", "Registers", "register-4"),
    builtin!("builtin/shift-register-4", "Registers", "shift-register-4"),
    builtin!("builtin/counter-4", "Counters", "counter-4"),
    builtin!("builtin/decade-counter", "Counters", "decade-counter"),
    builtin!("builtin/t-flip-flop", "Flip-flops", "t-flip-flop"),
    builtin!("builtin/bcd-7seg", "Display", "bcd-7seg"),
    builtin!("builtin/and-5", "Gates", "and-5"),
    builtin!("builtin/or-5", "Gates", "or-5"),
    builtin!("builtin/and-6", "Gates", "and-6"),
    builtin!("builtin/or-6", "Gates", "or-6"),
    builtin!("builtin/quad-nand2", "DIL packages", "quad-nand2"),
    builtin!("builtin/quad-and2", "DIL packages", "quad-and2"),
    builtin!("builtin/hex-inverter", "DIL packages", "hex-inverter"),
    builtin!("builtin/dual-dff", "DIL packages", "dual-dff"),
];

/// One part of the library.
#[derive(Debug, Clone)]
pub struct LibEntry {
    /// Stable key (`builtin/…` or `user/<relative path>`), recorded in
    /// imported definitions for "update from library".
    pub key: String,
    pub folder: String,
    pub name: String,
    pub file: MacroFile,
    /// File of a user part.
    pub path: Option<PathBuf>,
}

impl LibEntry {
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.path.is_none()
    }
}

static BUILTIN_PARTS: LazyLock<Vec<LibEntry>> = LazyLock::new(|| {
    BUILTIN
        .iter()
        .filter_map(|b| {
            let file = bwmacro::load_from_bytes(b.text.as_bytes()).ok()?;
            let name = file.def()?.name.clone();
            Some(LibEntry { key: b.key.into(), folder: b.folder.into(), name, file, path: None })
        })
        .collect()
});

/// The built-in parts (parsed once).
#[must_use]
pub fn builtin() -> &'static [LibEntry] {
    &BUILTIN_PARTS
}

/// The default user library folder,
/// `~/Library/Application Support/bitwerk/Parts`.
#[must_use]
pub fn user_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    home.is_absolute().then(|| home.join("Library/Application Support/bitwerk/Parts"))
}

/// Creates the folder if missing.
///
/// # Errors
/// I/O failure.
pub fn ensure_dir(dir: &Path) -> Result<(), IoError> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Maximum user part files scanned.
pub const MAX_USER_FILES: usize = 2000;
/// Maximum folder depth scanned below the library folder.
pub const MAX_USER_DEPTH: usize = 3;

/// A user library file that could not be loaded.
#[derive(Debug, Clone)]
pub struct BadPart {
    pub path: PathBuf,
    pub error: String,
}

/// Scans a user library folder (no symlinks, no hidden entries, bounded).
#[must_use]
pub fn scan(dir: &Path) -> (Vec<LibEntry>, Vec<BadPart>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut files = 0usize;
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = e.path();
            if ft.is_dir() {
                if depth < MAX_USER_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !ft.is_file() || path.extension().is_none_or(|x| !x.eq_ignore_ascii_case(MACRO_EXT)) {
                continue;
            }
            files += 1;
            if files > MAX_USER_FILES {
                return (ok, bad);
            }
            let rel = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
            let folder = rel.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            match bwmacro::load_from_path(&path) {
                Ok(file) => {
                    let name = file.def().map_or_else(String::new, |d| d.name.clone());
                    ok.push(LibEntry {
                        key: format!("user/{}", rel.to_string_lossy()),
                        folder: if folder.is_empty() { "My parts".into() } else { folder },
                        name,
                        file,
                        path: Some(path),
                    });
                }
                Err(e) => bad.push(BadPart { path, error: e.to_string() }),
            }
        }
    }
    ok.sort_by(|a, b| (&a.folder, &a.name).cmp(&(&b.folder, &b.name)));
    (ok, bad)
}

/// A file / folder name made safe: no separators, no leading dot, no
/// control characters, at most 64 bytes. `None` if nothing usable is left.
#[must_use]
pub fn sanitize_name(s: &str) -> Option<String> {
    let clean: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':'))
        .collect::<String>()
        .trim_start_matches('.')
        .trim()
        .to_string();
    let mut out = String::new();
    for ch in clean.chars() {
        if out.len() + ch.len_utf8() > 64 {
            break;
        }
        out.push(ch);
    }
    (!out.is_empty()).then_some(out)
}

fn folder_path(dir: &Path, folder: &str) -> Result<PathBuf, IoError> {
    if folder.is_empty() {
        return Ok(dir.to_path_buf());
    }
    let f = sanitize_name(folder).ok_or_else(|| IoError::Parse("invalid folder name".into()))?;
    Ok(dir.join(f))
}

/// Saves definition `root` of `table` as a user part `name` in `folder`
/// (refuses to overwrite an existing file).
///
/// # Errors
/// Invalid name, existing file, I/O or serialization failure.
pub fn save_user(dir: &Path, folder: &str, name: &str, table: &MacroTable, root: MacroId) -> Result<PathBuf, IoError> {
    let n = sanitize_name(name).ok_or_else(|| IoError::Parse("invalid part name".into()))?;
    let d = folder_path(dir, folder)?;
    ensure_dir(&d)?;
    let path = d.join(format!("{n}.{MACRO_EXT}"));
    if path.exists() {
        return Err(IoError::Parse(format!("{} already exists", path.display())));
    }
    bwmacro::save_to_path(table, root, &path)?;
    Ok(path)
}

/// Is `path` a part file inside `dir` (not following symlinks)?
fn inside(dir: &Path, path: &Path) -> bool {
    let (Ok(d), Ok(p)) = (dir.canonicalize(), path.canonicalize()) else { return false };
    p.starts_with(d)
        && path.extension().is_some_and(|x| x.eq_ignore_ascii_case(MACRO_EXT))
        && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// Renames a user part file.
///
/// # Errors
/// Not a part of the library, invalid name, target exists, I/O failure.
pub fn rename_user(dir: &Path, path: &Path, new_name: &str) -> Result<PathBuf, IoError> {
    if !inside(dir, path) {
        return Err(IoError::Parse("not a part of the user library".into()));
    }
    let n = sanitize_name(new_name).ok_or_else(|| IoError::Parse("invalid part name".into()))?;
    let to = path.with_file_name(format!("{n}.{MACRO_EXT}"));
    if to.exists() {
        return Err(IoError::Parse(format!("{} already exists", to.display())));
    }
    std::fs::rename(path, &to)?;
    Ok(to)
}

/// Moves a user part to another folder of the library.
///
/// # Errors
/// Not a part of the library, invalid folder, target exists, I/O failure.
pub fn move_user(dir: &Path, path: &Path, folder: &str) -> Result<PathBuf, IoError> {
    if !inside(dir, path) {
        return Err(IoError::Parse("not a part of the user library".into()));
    }
    let d = folder_path(dir, folder)?;
    ensure_dir(&d)?;
    let to = d.join(path.file_name().ok_or_else(|| IoError::Parse("bad path".into()))?);
    if to.exists() {
        return Err(IoError::Parse(format!("{} already exists", to.display())));
    }
    std::fs::rename(path, &to)?;
    Ok(to)
}

/// Deletes a user part file.
///
/// # Errors
/// Not a part of the library, I/O failure.
pub fn delete_user(dir: &Path, path: &Path) -> Result<(), IoError> {
    if !inside(dir, path) {
        return Err(IoError::Parse("not a part of the user library".into()));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_files_parse() {
        for b in BUILTIN {
            if let Err(e) = bwmacro::load_from_bytes(b.text.as_bytes()) {
                panic!("{}: {e}", b.key);
            }
        }
        assert_eq!(builtin().len(), BUILTIN.len());
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("../../etc/passwd").as_deref(), Some("etcpasswd"));
        assert_eq!(sanitize_name(" .hidden ").as_deref(), Some("hidden"));
        assert_eq!(sanitize_name("a\0b\nc").as_deref(), Some("abc"));
        assert_eq!(sanitize_name("///"), None);
        assert!(sanitize_name(&"x".repeat(500)).is_some_and(|s| s.len() == 64));
    }
}

/// A large design made of macros (debug stress test / benchmarks): `cells`
/// instances of a "Cell" macro (a 4-bit counter feeding a 4-bit adder, so
/// four levels deep: cell › adder › full adder › half adder), in rows that
/// share a clock, an enable and a clear.
///
/// # Errors
/// Only if the built-in parts are missing or a limit is exceeded.
#[allow(clippy::many_single_char_names, reason = "short local names for pins / components")]
pub fn stress_macros(cells: usize) -> Result<bw_core::Circuit, bw_core::CircuitError> {
    use std::sync::Arc;

    use bw_core::{Circuit, ComponentKind, Edit, GridPos, MacroDef, PinRef, Rotation, TagDir, WireEnd};

    let mut c = Circuit::new();
    let import = |c: &mut Circuit, key: &str| -> Result<MacroId, bw_core::CircuitError> {
        let e = builtin()
            .iter()
            .find(|e| e.key == key)
            .ok_or_else(|| bw_core::CircuitError::BadMacro(format!("missing built-in {key}")))?;
        let (defs, root) = c.macros().import_plan(&e.file.table, e.file.root)?;
        for (id, def) in defs {
            c.apply(&Edit::DefineMacro { id, def })?;
        }
        Ok(root)
    };
    let counter = import(&mut c, "builtin/counter-4")?;
    let adder = import(&mut c, "builtin/adder-4")?;
    let shape = |c: &Circuit, id: MacroId| c.macros().get(id).map(|e| ComponentKind::Macro(e.shape));
    let wire = |b: &mut Circuit, f: PinRef, t: PinRef| {
        b.apply(&Edit::AddWire { from: WireEnd::Pin(f), to: WireEnd::Pin(t), via: Vec::new() }).map(|_| ())
    };
    let p = |component, pin| PinRef { component, pin };
    let mut d = MacroDef::new("Cell");
    let b = &mut d.body;
    let tag = |dir| ComponentKind::Tag { dir, tristate: false };
    let en = b.add_component(tag(TagDir::In), GridPos::new(0, 0), Rotation::R0)?;
    let clk = b.add_component(tag(TagDir::In), GridPos::new(0, 4), Rotation::R0)?;
    let clr = b.add_component(tag(TagDir::In), GridPos::new(0, 8), Rotation::R0)?;
    let cnt = b.add_component(
        shape(&c, counter).ok_or(bw_core::CircuitError::NoSuchMacro(counter))?,
        GridPos::new(12, 0),
        Rotation::R0,
    )?;
    let add = b.add_component(
        shape(&c, adder).ok_or(bw_core::CircuitError::NoSuchMacro(adder))?,
        GridPos::new(40, 0),
        Rotation::R0,
    )?;
    wire(b, p(en, 0), p(cnt, 0))?;
    wire(b, p(clk, 0), p(cnt, 1))?;
    wire(b, p(clr, 0), p(cnt, 2))?;
    for k in 0..4u8 {
        wire(b, p(cnt, 3 + k), p(add, k))?;
        wire(b, p(cnt, 3 + k), p(add, 4 + 3 - k))?;
    }
    wire(b, p(cnt, 7), p(add, 8))?;
    for k in 0..5u8 {
        let t = b.add_component(tag(TagDir::Out), GridPos::new(80, 4 * i32::from(k)), Rotation::R0)?;
        wire(b, p(add, 9 + k), p(t, 0))?;
    }
    d.sync_pins();
    let cell = c.macros().free_id().ok_or(bw_core::CircuitError::TooManyMacros)?;
    c.apply(&Edit::DefineMacro { id: cell, def: Arc::new(d) })?;
    let kind = shape(&c, cell).ok_or(bw_core::CircuitError::NoSuchMacro(cell))?;
    let (cw, ch) = kind.size();
    let per_row = 50usize;
    let rows = cells.div_ceil(per_row);
    for row in 0..rows {
        let y = i32::try_from(row).unwrap_or(0) * (ch + 6);
        let half = 2 + u32::try_from(row % 5).unwrap_or(0);
        let ck = c.add_component(ComponentKind::Clock { half_period: half }, GridPos::new(0, y), Rotation::R0)?;
        let on = c.add_component(ComponentKind::Switch { on: true }, GridPos::new(0, y + 3), Rotation::R0)?;
        let off = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, y + 6), Rotation::R0)?;
        let mut prev: Option<bw_core::ComponentId> = None;
        for col in 0..per_row.min(cells - row * per_row) {
            let x = 10 + i32::try_from(col).unwrap_or(0) * (cw + 10);
            let inst = c.add_component(kind, GridPos::new(x, y), Rotation::R0)?;
            let (a, bb, cc) = match prev {
                None => (p(on, 0), p(ck, 0), p(off, 0)),
                Some(q) => (p(q, 0), p(q, 1), p(q, 2)),
            };
            wire(&mut c, a, p(inst, 0))?;
            wire(&mut c, bb, p(inst, 1))?;
            wire(&mut c, cc, p(inst, 2))?;
            prev = Some(inst);
        }
    }
    Ok(c)
}

#[cfg(test)]
mod stress {
    #[test]
    fn stress_macros_builds_a_deep_design() {
        let c = super::stress_macros(120).unwrap();
        c.check().unwrap();
        assert_eq!(c.component_count(), 120 + 3 * 3);
        let cell = c.macros().iter().find(|(_, e)| e.def.name == "Cell").unwrap().1;
        assert_eq!(cell.depth, 4, "cell › adder › full adder › half adder");
        assert!(c.flat_size() > 120 * 25);
    }
}
