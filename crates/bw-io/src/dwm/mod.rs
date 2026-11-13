//! Digital Works (`.dwm`) import: circuits and macros of Digital Works 2.x /
//! 3.x (format versions 20, 30 and 34), templates (`.dwt`) and, through
//! [`crate::memmap`], memory maps (`.map`).
//!
//! The format is documented in `docs/dwm/DWM-FORMAT.md` (derived from the
//! original program, read as data only). Import has two stages:
//!
//! 1. [`reader`] parses the object stream into an object table (syntax
//!    only, iterative, every size bounded);
//! 2. [`convert`] resolves and type-checks the references and builds the
//!    bitwerk model (components, wire graph, macro definitions
//!    deduplicated by content), collecting an [`ImportReport`]: what was
//!    imported, what was approximated, repaired or left out. Nothing is
//!    dropped silently.
//!
//! The resulting circuit goes through the same validation as a project
//! file. Imported projects use the Digital Works compatible simulation mode
//! ([`bw_core::EvalMode::Sequential`]).
//!
//! Export is the reverse: [`export`] builds the object graph of a circuit
//! or macro (collecting an [`ExportReport`] of what had to be approximated
//! or dropped) and [`writer`] serialises it with the reader's own field
//! programs. Every exported file is parsed again before it is returned.

pub mod convert;
pub mod export;
pub mod reader;
pub mod writer;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bwmacro::MacroFile;
use crate::{IoError, Project};

/// Severity class of an import message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Imported with an approximation (geometry, colour, semantics).
    Approximation,
    /// Data in the file that was ignored or repaired (unknown values,
    /// stale references).
    Warning,
    /// A feature bitwerk does not have: imported as something else or left
    /// out (always listed, never dropped silently).
    Unsupported,
}

/// One kind of import message with how often it occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportNote {
    pub severity: Severity,
    pub message: String,
    pub count: usize,
}

/// What an import did.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    /// File name (no directories).
    pub source: String,
    /// Format version byte of the file (20, 30 or 34).
    pub version: u8,
    /// Circuit / macro name (header description, else the file name).
    pub name: String,
    /// The Digital Works datasheet path, if any (kept as a note, never
    /// opened).
    pub datasheet: Option<String>,
    /// Imported objects by category ("AND gates", "wires", "junctions",
    /// "macro instances", …), in display order.
    pub counts: Vec<(String, usize)>,
    /// Embedded macro copies in the file and distinct definitions made.
    pub macro_copies: usize,
    pub macro_defs: usize,
    pub notes: Vec<ImportNote>,
    pub elapsed: Duration,
}

impl ImportReport {
    /// Plain-text report (for the report window and the clipboard).
    #[must_use]
    pub fn to_text(&self) -> String {
        use std::fmt::Write as _;
        let mut t = String::new();
        let _ = writeln!(t, "Digital Works import: {}", self.source);
        let _ = writeln!(t, "Name: {}", self.name);
        let _ = writeln!(t, "Format version: {}", self.version);
        if let Some(ds) = &self.datasheet {
            let _ = writeln!(t, "Datasheet (not opened): {ds}");
        }
        let _ = writeln!(t, "Time: {:.1} ms", self.elapsed.as_secs_f64() * 1000.0);
        let _ = writeln!(t, "\nImported:");
        for (what, n) in &self.counts {
            let _ = writeln!(t, "  {n:>7}  {what}");
        }
        if self.macro_copies > 0 {
            let _ = writeln!(t, "  {:>7}  embedded macro copies → {} definitions", self.macro_copies, self.macro_defs);
        }
        for (sev, title) in [
            (Severity::Unsupported, "Unsupported (imported differently or left out)"),
            (Severity::Warning, "Warnings (ignored or repaired data)"),
            (Severity::Approximation, "Approximations"),
        ] {
            let list: Vec<&ImportNote> = self.notes.iter().filter(|n| n.severity == sev).collect();
            if list.is_empty() {
                continue;
            }
            let _ = writeln!(t, "\n{title}:");
            for n in list {
                let _ = writeln!(t, "  {:>5} ×  {}", n.count, n.message);
            }
        }
        t
    }

    /// Number of notes of a severity (sum of counts).
    #[must_use]
    pub fn total(&self, sev: Severity) -> usize {
        self.notes.iter().filter(|n| n.severity == sev).map(|n| n.count).sum()
    }
}

/// What an export did.
#[derive(Debug, Clone, Default)]
pub struct ExportReport {
    /// Target file name (no directories).
    pub target: String,
    /// Circuit / macro name written to the header.
    pub name: String,
    /// Format version written (34).
    pub version: u8,
    /// Written objects by category, in display order.
    pub counts: Vec<(String, usize)>,
    /// Embedded macro copies written and distinct definitions.
    pub macro_copies: usize,
    pub macro_defs: usize,
    /// Objects in the stream and file size.
    pub objects: usize,
    pub bytes: usize,
    pub notes: Vec<ImportNote>,
    pub elapsed: Duration,
}

impl ExportReport {
    /// Plain-text report (for the report window and the clipboard).
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut t = String::new();
        let _ = writeln!(t, "Digital Works export: {}", self.target);
        let _ = writeln!(t, "Name: {}", self.name);
        let _ = writeln!(t, "Format version: {} (Digital Works 3.x)", self.version);
        let _ = writeln!(t, "Size: {} bytes, {} objects", self.bytes, self.objects);
        let _ = writeln!(t, "Time: {:.1} ms", self.elapsed.as_secs_f64() * 1000.0);
        let _ = writeln!(t, "\nWritten:");
        for (what, n) in &self.counts {
            let _ = writeln!(t, "  {n:>7}  {what}");
        }
        if self.macro_copies > 0 {
            let _ = writeln!(t, "  {:>7}  embedded macro copies of {} definitions", self.macro_copies, self.macro_defs);
        }
        for (sev, title) in [
            (Severity::Unsupported, "Dropped (Digital Works cannot represent it)"),
            (Severity::Warning, "Warnings"),
            (Severity::Approximation, "Approximations"),
        ] {
            let list: Vec<&ImportNote> = self.notes.iter().filter(|n| n.severity == sev).collect();
            if list.is_empty() {
                continue;
            }
            let _ = writeln!(t, "\n{title}:");
            for n in list {
                let _ = writeln!(t, "  {:>5} x  {}", n.count, n.message);
            }
        }
        if self.notes.is_empty() {
            let _ = writeln!(t, "\nNothing was approximated or dropped.");
        }
        t
    }

    /// Number of notes of a severity (sum of counts).
    #[must_use]
    pub fn total(&self, sev: Severity) -> usize {
        self.notes.iter().filter(|n| n.severity == sev).map(|n| n.count).sum()
    }
}

/// Serialises a built graph, checks that it reads back, and makes the
/// report.
fn finish_export(
    built: Result<(reader::Graph, export::Exporter<'_>), export::ExportError>,
    file_name: &str,
    t0: std::time::Instant,
) -> Result<(Vec<u8>, ExportReport), IoError> {
    let (g, ex) = built.map_err(|e| IoError::Dwm(e.0))?;
    let bytes = writer::write(&g).map_err(|e| IoError::Dwm(e.0))?;
    // Never hand out a file our own reader would refuse.
    let back = reader::parse(&bytes)
        .map_err(|e| IoError::Dwm(format!("internal error: exported file does not read back: {e}")))?;
    let report = ExportReport {
        target: file_stem_full(file_name),
        name: g.header.name.clone(),
        version: g.header.version,
        counts: export::ordered_counts(&ex.notes),
        macro_copies: ex.copies,
        macro_defs: ex.definitions(),
        objects: back.objects.len(),
        bytes: bytes.len(),
        notes: ex.notes.into_list(),
        elapsed: t0.elapsed(),
    };
    Ok((bytes, report))
}

/// Exports a project as a Digital Works 3 circuit (`.dwm`, version 34).
/// Lossy: see the report.
///
/// # Errors
/// The circuit is too large for a Digital Works file (macros are copied
/// per instance), or an internal consistency failure.
pub fn export_project(project: &Project, file_name: &str) -> Result<(Vec<u8>, ExportReport), IoError> {
    export_circuit(&project.circuit, &project.name, file_name)
}

/// [`export_project`] for a circuit and its name (no copy of the circuit
/// needed).
///
/// # Errors
/// As [`export_project`].
pub fn export_circuit(
    circuit: &bw_core::Circuit,
    name: &str,
    file_name: &str,
) -> Result<(Vec<u8>, ExportReport), IoError> {
    let t0 = std::time::Instant::now();
    let name = if name.trim().is_empty() { file_stem(file_name).to_string() } else { name.to_string() };
    finish_export(export::project_graph(circuit, &name), file_name, t0)
}

/// Exports macro definition `root` of `table` as a Digital Works macro
/// (`.dwm` whose tags are the interface).
///
/// # Errors
/// As [`export_project`]; an unknown definition.
pub fn export_macro(
    table: &bw_core::MacroTable,
    root: bw_core::MacroId,
    file_name: &str,
) -> Result<(Vec<u8>, ExportReport), IoError> {
    let t0 = std::time::Instant::now();
    finish_export(export::macro_graph(table, root), file_name, t0)
}

/// Is this a Digital Works circuit / macro file (by its magic number)?
#[must_use]
pub fn is_dwm(bytes: &[u8]) -> bool {
    bytes.len() >= 5 && bytes[0] == 4 && bytes[1] == 0xdd && bytes[2] == 0xc4 && bytes[4] == 0
}

/// Imports a `.dwm` file as a new project (root circuit = the file's
/// circuit; simulation mode = Digital Works compatible).
///
/// # Errors
/// Unreadable / malformed / hostile files, or limits exceeded.
pub fn import_project(bytes: &[u8], file_name: &str) -> Result<(Project, ImportReport), IoError> {
    let t0 = std::time::Instant::now();
    let g = parse(bytes)?;
    let mut conv = convert::Converter::new(&g);
    let circuit = conv.root_circuit().map_err(|e| IoError::Dwm(e.0))?;
    let name = project_name(&g, file_name);
    let report = finish_report(conv, &g, file_name, &name, t0);
    let project = Project { name: clip_name(&name, crate::MAX_NAME_LEN), circuit };
    crate::check_limits(&project.circuit, &project.name)?;
    Ok((project, report))
}

/// Parses the stream, mapping syntax errors to [`IoError::Dwm`].
fn parse(bytes: &[u8]) -> Result<reader::Graph, IoError> {
    if bytes.len() as u64 > crate::MAX_FILE_SIZE {
        return Err(IoError::TooLarge);
    }
    let g = reader::parse(bytes).map_err(|e| IoError::Dwm(e.to_string()))?;
    if g.header.template {
        return Err(IoError::Dwm("this is a template (.dwt), not a circuit".into()));
    }
    Ok(g)
}

fn file_stem(file_name: &str) -> &str {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    base.rsplit_once('.').map_or(base, |(a, _)| a)
}

fn project_name(g: &reader::Graph, file_name: &str) -> String {
    let h = g.header.name.trim();
    let stem = file_stem(file_name);
    if !h.is_empty() {
        h.to_string()
    } else if !stem.is_empty() {
        stem.to_string()
    } else {
        "Imported circuit".into()
    }
}

fn clip_name(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

fn finish_report(
    conv: convert::Converter<'_>,
    g: &reader::Graph,
    file_name: &str,
    name: &str,
    t0: std::time::Instant,
) -> ImportReport {
    let macro_defs = conv.table.len();
    // Every embedded macro object of the file (the root excluded).
    let copies = g.objects.iter().filter(|o| o.cls() == reader::Cls::Macro).count().saturating_sub(1);
    let _ = conv.copies;
    let mut notes = conv.notes;
    for (id, v) in &g.header.other {
        if !matches!((id, v.as_str()), (102, "0" | "") | (200 | 202, "0" | "1")) {
            let shown = if v.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
                format!("{v:?}")
            } else {
                format!("({} bytes of binary data)", v.len())
            };
            notes.add(Severity::Warning, format!("unknown header property {id} = {shown} ignored"));
        }
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for what in convert::COUNT_ORDER {
        if let Some(&n) = notes.counts.get(what) {
            counts.push(((*what).to_string(), n));
        }
    }
    let datasheet = (!g.header.datasheet.trim().is_empty()).then(|| g.header.datasheet.clone());
    ImportReport {
        source: file_stem_full(file_name),
        version: g.header.version,
        name: name.to_string(),
        datasheet,
        counts,
        macro_copies: copies,
        macro_defs,
        notes: notes.into_list(),
        elapsed: t0.elapsed(),
    }
}

fn file_stem_full(file_name: &str) -> String {
    file_name.rsplit(['/', '\\']).next().unwrap_or(file_name).to_string()
}

/// Imports a `.dwm` file as a macro (its tags become the interface).
///
/// # Errors
/// As [`import_project`]; also a file without tags (not a macro).
pub fn import_macro(bytes: &[u8], file_name: &str) -> Result<(MacroFile, ImportReport), IoError> {
    let t0 = std::time::Instant::now();
    let g = parse(bytes)?;
    let mut conv = convert::Converter::new(&g);
    let name = clip_name(&project_name(&g, file_name), bw_core::MAX_MACRO_NAME);
    let mut datasheet = String::from("Imported from Digital Works.");
    if !g.header.datasheet.trim().is_empty() {
        let _ = write!(datasheet, "\nOriginal datasheet file: {}", g.header.datasheet.trim());
    }
    let root = conv.root_macro(&name, &datasheet).map_err(|e| IoError::Dwm(e.0))?;
    let table = conv.table.clone();
    let report = finish_report(conv, &g, file_name, &name, t0);
    Ok((MacroFile { table, root }, report))
}

/// Result of importing a folder of `.dwm` macros into the user library.
#[derive(Debug, Clone, Default)]
pub struct FolderImport {
    /// Library files written.
    pub written: Vec<PathBuf>,
    /// Files whose macro was already in the library (same content hash).
    pub duplicates: Vec<PathBuf>,
    /// Files that are circuits without tags (not macros), skipped.
    pub not_macros: Vec<PathBuf>,
    /// Files that could not be imported, with the error.
    pub failed: Vec<(PathBuf, String)>,
    /// The per-file reports of the imported files.
    pub reports: Vec<ImportReport>,
    /// The user cancelled (the files written so far stay).
    pub cancelled: bool,
}

/// Imports every `.dwm` below `src` (no symlinks, bounded depth and count)
/// as `.bwmacro` files into `dest` (normally `<user library>/Imported`),
/// keeping the sub-folder names, skipping macros whose content is already
/// in `dest`. `progress(done, total, current)` is called before each file;
/// returning `false` cancels.
///
/// # Errors
/// Only if `dest` cannot be created; per-file problems are in the result.
pub fn import_folder(
    src: &Path,
    dest: &Path,
    progress: &mut dyn FnMut(usize, usize, &Path) -> bool,
) -> Result<FolderImport, IoError> {
    crate::library::ensure_dir(dest)?;
    let files = find_dwm(src);
    // Content hashes already in the destination (re-imports skip them).
    let (existing, _) = crate::library::scan(dest);
    let mut known: std::collections::HashSet<u64> = existing.iter().map(|e| e.file.hash()).collect();
    let mut out = FolderImport::default();
    let total = files.len();
    for (i, path) in files.iter().enumerate() {
        if !progress(i, total, path) {
            out.cancelled = true;
            return Ok(out);
        }
        let bytes = match crate::read_capped(path) {
            Ok(b) => b,
            Err(e) => {
                out.failed.push((path.clone(), e.to_string()));
                continue;
            }
        };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match import_macro(&bytes, &name) {
            Ok((file, report)) => {
                if !known.insert(file.hash()) {
                    out.duplicates.push(path.clone());
                    continue;
                }
                let rel = path.strip_prefix(src).ok().and_then(Path::parent).unwrap_or(Path::new(""));
                match write_part(dest, rel, file_stem(&name), &file) {
                    Ok(p) => {
                        out.written.push(p);
                        out.reports.push(report);
                    }
                    Err(e) => out.failed.push((path.clone(), e.to_string())),
                }
            }
            Err(IoError::Dwm(e)) if e.starts_with("not a macro") => out.not_macros.push(path.clone()),
            Err(e) => out.failed.push((path.clone(), e.to_string())),
        }
    }
    progress(total, total, src);
    Ok(out)
}

/// Most files a folder import looks at, and how deep.
pub const MAX_FOLDER_FILES: usize = 5000;
pub const MAX_FOLDER_DEPTH: usize = 8;

/// `.dwm` files below `dir` (sorted; no symlinks, no hidden entries).
fn find_dwm(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries.into_iter().rev() {
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = e.path();
            if ft.is_dir() {
                if depth < MAX_FOLDER_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if ft.is_file() && path.extension().is_some_and(|x| x.eq_ignore_ascii_case("dwm")) {
                out.push(path);
                if out.len() >= MAX_FOLDER_FILES {
                    out.sort();
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}

/// Writes one imported part below `dest`, keeping (at most two levels of)
/// the source sub-folders; never overwrites.
fn write_part(dest: &Path, rel: &Path, stem: &str, file: &MacroFile) -> Result<PathBuf, IoError> {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => crate::library::sanitize_name(&s.to_string_lossy()),
            _ => None,
        })
        .collect();
    let mut dir = dest.to_path_buf();
    match parts.len() {
        0 => {}
        1 => dir.push(&parts[0]),
        _ => {
            dir.push(&parts[0]);
            if let Some(rest) = crate::library::sanitize_name(&parts[1..].join(" - ")) {
                dir.push(rest);
            }
        }
    }
    crate::library::ensure_dir(&dir)?;
    let base = crate::library::sanitize_name(stem).unwrap_or_else(|| "part".into());
    let mut path = dir.join(format!("{base}.{}", crate::bwmacro::MACRO_EXT));
    let mut k = 2;
    while path.exists() {
        path = dir.join(format!("{base} ({k}).{}", crate::bwmacro::MACRO_EXT));
        k += 1;
        if k > 1000 {
            return Err(IoError::Parse("too many files with the same name".into()));
        }
    }
    crate::bwmacro::save_to_path(&file.table, file.root, &path)?;
    Ok(path)
}

/// Pin layout of a Digital Works template (`.dwt`): the pin positions (grid
/// units) in order and the package it matches.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateInfo {
    pub pins: Vec<(f32, f32)>,
    pub package: bw_core::Package,
}

/// Reads a `.dwt` template.
///
/// # Errors
/// Not a template, or a malformed file.
pub fn read_template(bytes: &[u8]) -> Result<TemplateInfo, IoError> {
    let g = reader::parse(bytes).map_err(|e| IoError::Dwm(e.to_string()))?;
    if !g.header.template {
        return Err(IoError::Dwm("not a template".into()));
    }
    let mut pins = Vec::new();
    for r in g.roots.iter().skip(1) {
        for p in g.items(*r) {
            let Some(o) = g.get(p) else { continue };
            if !o.cls().is_pin() {
                continue;
            }
            let pts = g.points(o.r(reader::F::Points));
            if pts.is_empty() {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let n = pts.len() as f32;
            let lo = pts.iter().fold((f32::MAX, f32::MAX), |a, p| (a.0.min(p.0), a.1.min(p.1)));
            let hi = pts.iter().fold((f32::MIN, f32::MIN), |a, p| (a.0.max(p.0), a.1.max(p.1)));
            let _ = n;
            pins.push(((lo.0 + hi.0) / 10.0, (lo.1 + hi.1) / 10.0));
        }
    }
    let package = convert::dil_package_of(&pins);
    Ok(TemplateInfo { pins, package })
}

/// Parses a legacy `.dwm` file into a project (see [`import_project`]).
///
/// # Errors
/// As [`import_project`].
pub fn load(bytes: &[u8]) -> Result<Project, IoError> {
    import_project(bytes, "").map(|(p, _)| p)
}
