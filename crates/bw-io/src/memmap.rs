//! Memory map files (`.map`): the contents of one RAM / ROM as plain text,
//! for the Memory Contents window's Load and Save.
//!
//! ```text
//! # bitwerk memory map           (comment lines start with '#' or ';')
//! bitwerk-map 1                  (magic and version, first line)
//! width 8                        (bits per word, 1..=16)
//! words 256                      (number of words, 1..=16384)
//! 0000: 3e 00 c3 10 ...          (address: words, all hexadecimal)
//! 0010: ...
//! ```
//!
//! Data lines start with the hexadecimal address of their first word and a
//! colon, followed by words in hexadecimal separated by spaces. Addresses
//! must increase (a gap leaves zeros); words not listed are 0. The writer
//! puts 16 words per line.
//!
//! Parsing treats the file as hostile input: total size, line length,
//! token sizes, word count and values are all bounded and checked; nothing
//! is allocated beyond `words`.
//!
//! [`parse`] also reads the binary memory maps of Digital Works (detected by
//! their magic number, see [`parse_dw`]): `u32` magic `0xABCD`, `u32` word
//! count, then that many `u32` words (little endian; only the low 16 bits
//! are meaningful).

use std::fmt::Write as _;
use std::path::Path;

use bw_core::memenc::width_mask;
use bw_core::{MAX_MEMORY_WORDS, data_width_index};
use thiserror::Error;

/// Magic first line (followed by the version).
pub const MAP_MAGIC: &str = "bitwerk-map";
pub const MAP_VERSION: u32 = 1;
/// Largest accepted `.map` file.
pub const MAX_MAP_SIZE: usize = 1024 * 1024;
/// Longest accepted line.
pub const MAX_LINE_LEN: usize = 4096;
/// Words per line written by [`to_string`].
const WORDS_PER_LINE: usize = 16;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MapError {
    #[error("this is not a valid memory map file")]
    NotAMap,
    #[error("memory map version {0} is not supported")]
    Version(u32),
    #[error("memory map is too large")]
    TooLarge,
    #[error("line {line}: {msg}")]
    Line { line: usize, msg: &'static str },
    #[error("memory map has no {0} line")]
    Missing(&'static str),
    #[error("i/o error: {0}")]
    Io(String),
}

/// Contents of one memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMap {
    /// Bits per word (1..=16; any width is accepted in files, the app
    /// masks to the device's width).
    pub data_bits: u8,
    pub words: Vec<u16>,
}

fn hex(tok: &str, max_digits: usize) -> Option<u32> {
    if tok.is_empty() || tok.len() > max_digits || !tok.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(tok, 16).ok()
}

/// Magic number of a Digital Works memory map.
pub const DW_MAP_MAGIC: u32 = 0xABCD;

/// Is this a Digital Works (binary) memory map?
#[must_use]
pub fn is_dw_map(bytes: &[u8]) -> bool {
    bytes.get(..4).is_some_and(|m| u32::from_le_bytes([m[0], m[1], m[2], m[3]]) == DW_MAP_MAGIC)
}

/// Parses a Digital Works binary memory map: magic, count, `count` words.
/// The file must be exactly that long. Words keep their low 16 bits (the
/// map is 16 bits wide; the app masks it to the device's width and
/// truncates or zero-fills it to the device's size, as Digital Works does).
///
/// # Errors
/// Bad magic, too large, or a length that does not match the count.
pub fn parse_dw(bytes: &[u8]) -> Result<MemoryMap, MapError> {
    if bytes.len() > MAX_MAP_SIZE {
        return Err(MapError::TooLarge);
    }
    if !is_dw_map(bytes) || bytes.len() < 8 {
        return Err(MapError::NotAMap);
    }
    let count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let body = &bytes[8..];
    if count.checked_mul(4) != Some(body.len()) {
        return Err(MapError::Line { line: 1, msg: "word count does not match the file length" });
    }
    if count == 0 {
        return Err(MapError::Missing("words"));
    }
    #[allow(clippy::cast_possible_truncation, reason = "only the low 16 bits are meaningful")]
    let words = body.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u16).collect();
    Ok(MemoryMap { data_bits: 16, words })
}

/// Writes a Digital Works binary memory map.
#[must_use]
pub fn to_dw_bytes(map: &MemoryMap) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 4 * map.words.len());
    out.extend_from_slice(&DW_MAP_MAGIC.to_le_bytes());
    out.extend_from_slice(&u32::try_from(map.words.len()).unwrap_or(u32::MAX).to_le_bytes());
    for &w in &map.words {
        out.extend_from_slice(&u32::from(w).to_le_bytes());
    }
    out
}

/// Parses a memory map from untrusted bytes (bitwerk text or Digital Works
/// binary, detected by the magic number).
///
/// # Errors
/// Any violation of the format or its limits.
pub fn parse(bytes: &[u8]) -> Result<MemoryMap, MapError> {
    if bytes.len() > MAX_MAP_SIZE {
        return Err(MapError::TooLarge);
    }
    if is_dw_map(bytes) {
        return parse_dw(bytes);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| MapError::NotAMap)?;
    let mut magic = false;
    let mut width: Option<u8> = None;
    let mut count: Option<usize> = None;
    let mut words: Vec<u16> = Vec::new();
    let mut next_addr = 0usize;
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let err = |msg| MapError::Line { line: line_no, msg };
        if raw.len() > MAX_LINE_LEN {
            return Err(err("line too long"));
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if !magic {
            let mut it = line.split_ascii_whitespace();
            if it.next() != Some(MAP_MAGIC) {
                return Err(MapError::NotAMap);
            }
            let v = it.next().and_then(|v| v.parse::<u32>().ok()).ok_or(MapError::NotAMap)?;
            if v != MAP_VERSION {
                return Err(MapError::Version(v));
            }
            if it.next().is_some() {
                return Err(MapError::NotAMap);
            }
            magic = true;
            continue;
        }
        if let Some((addr, rest)) = line.split_once(':') {
            let (Some(bits), Some(n)) = (width, count) else {
                return Err(err("data before the width and words lines"));
            };
            let addr = hex(addr.trim(), 4).ok_or(err("bad address"))? as usize;
            if addr < next_addr || addr >= n {
                return Err(err("address out of order or out of range"));
            }
            words.resize(addr, 0);
            for tok in rest.split_ascii_whitespace() {
                let v = hex(tok, 4).ok_or(err("bad word"))?;
                if v > u32::from(width_mask(bits)) {
                    return Err(err("word wider than the data width"));
                }
                if words.len() >= n {
                    return Err(err("more words than declared"));
                }
                #[allow(clippy::cast_possible_truncation, reason = "checked against the 16-bit mask")]
                words.push(v as u16);
            }
            next_addr = words.len();
            continue;
        }
        let mut it = line.split_ascii_whitespace();
        let (key, val) = (it.next(), it.next());
        if it.next().is_some() {
            return Err(err("unexpected text"));
        }
        let val = val.and_then(|v| if v.len() <= 6 { v.parse::<usize>().ok() } else { None });
        match (key, val) {
            (Some("width"), Some(v)) if width.is_none() && (1..=16).contains(&v) => {
                width = u8::try_from(v).ok();
            }
            (Some("words"), Some(v)) if count.is_none() && (1..=MAX_MEMORY_WORDS).contains(&v) => count = Some(v),
            _ => return Err(err("expected 'width N', 'words N' or 'address: words'")),
        }
    }
    if !magic {
        return Err(MapError::NotAMap);
    }
    let data_bits = width.ok_or(MapError::Missing("width"))?;
    let n = count.ok_or(MapError::Missing("words"))?;
    words.resize(n, 0);
    Ok(MemoryMap { data_bits, words })
}

/// Writes a memory map (see the module docs).
#[must_use]
pub fn to_string(map: &MemoryMap) -> String {
    let digits = usize::from(map.data_bits.clamp(1, 16)).div_ceil(4);
    let mask = width_mask(map.data_bits.clamp(1, 16));
    let mut out = String::with_capacity(64 + map.words.len() * (digits + 1) + map.words.len() / 16 * 7);
    let _ = writeln!(out, "# bitwerk memory map");
    let _ = writeln!(out, "{MAP_MAGIC} {MAP_VERSION}");
    let _ = writeln!(out, "width {}", map.data_bits);
    let _ = writeln!(out, "words {}", map.words.len());
    for (row, chunk) in map.words.chunks(WORDS_PER_LINE).enumerate() {
        let _ = write!(out, "{:04x}:", row * WORDS_PER_LINE);
        for w in chunk {
            let _ = write!(out, " {:0digits$x}", w & mask);
        }
        out.push('\n');
    }
    out
}

/// Reads a memory map file (size checked before and while reading).
///
/// # Errors
/// I/O errors and everything [`parse`] rejects.
pub fn load_path(path: &Path) -> Result<MemoryMap, MapError> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|e| MapError::Io(e.to_string()))?;
    let meta = file.metadata().map_err(|e| MapError::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(MapError::NotAMap);
    }
    if meta.len() > MAX_MAP_SIZE as u64 {
        return Err(MapError::TooLarge);
    }
    let mut buf = Vec::new();
    file.take(MAX_MAP_SIZE as u64 + 1).read_to_end(&mut buf).map_err(|e| MapError::Io(e.to_string()))?;
    parse(&buf)
}

/// Writes a memory map file atomically (temp file + rename).
///
/// # Errors
/// I/O failure or an invalid width.
pub fn save_path(map: &MemoryMap, path: &Path) -> Result<(), MapError> {
    use std::io::Write as _;
    if data_width_index(map.data_bits).is_none() && !(1..=16).contains(&map.data_bits) {
        return Err(MapError::Missing("width"));
    }
    let text = to_string(map);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let io = |e: std::io::Error| MapError::Io(e.to_string());
    {
        let mut f = std::fs::File::create(&tmp).map_err(io)?;
        f.write_all(text.as_bytes()).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(io)
}

/// Writes a Digital Works binary memory map atomically (temp file +
/// rename).
///
/// # Errors
/// I/O failure.
pub fn save_dw_path(map: &MemoryMap, path: &Path) -> Result<(), MapError> {
    crate::write_bytes_atomic(&to_dw_bytes(map), path).map_err(|e| MapError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for (bits, n) in [(1u8, 16usize), (4, 16), (8, 256), (16, MAX_MEMORY_WORDS)] {
            let mask = width_mask(bits);
            #[allow(clippy::cast_possible_truncation)]
            let words: Vec<u16> = (0..n).map(|i| (i as u16).wrapping_mul(40_503) & mask).collect();
            let map = MemoryMap { data_bits: bits, words };
            let text = to_string(&map);
            assert_eq!(parse(text.as_bytes()).unwrap(), map, "{bits} bits x {n}");
        }
    }

    #[test]
    fn accepts_comments_gaps_and_short_data() {
        let text = "; hand written\nbitwerk-map 1\n\nwords 32\nwidth 4\n0000: 1 2 3\n# skip\n0010: f\n";
        let m = parse(text.as_bytes()).unwrap();
        assert_eq!(m.data_bits, 4);
        assert_eq!(m.words.len(), 32);
        assert_eq!(&m.words[..4], &[1, 2, 3, 0]);
        assert_eq!(m.words[16], 15);
    }

    #[test]
    fn rejects_malformed_maps() {
        let head = "bitwerk-map 1\nwidth 8\nwords 16\n";
        let cases: Vec<(String, &str)> = vec![
            (String::new(), "empty"),
            ("hello\n".into(), "wrong magic"),
            ("bitwerk-map 2\nwidth 8\nwords 16\n".into(), "future version"),
            ("bitwerk-map 1\nwords 16\n".into(), "no width"),
            ("bitwerk-map 1\nwidth 8\n".into(), "no words"),
            ("bitwerk-map 1\nwidth 17\nwords 16\n".into(), "width too large"),
            ("bitwerk-map 1\nwidth 0\nwords 16\n".into(), "width zero"),
            ("bitwerk-map 1\nwidth 8\nwords 16385\n".into(), "too many words"),
            ("bitwerk-map 1\nwidth 8\nwidth 8\nwords 16\n".into(), "duplicate width"),
            ("bitwerk-map 1\n0000: 1\nwidth 8\nwords 16\n".into(), "data before header"),
            (format!("{head}0000: 100\n"), "word too wide"),
            (format!("{head}0000: zz\n"), "not hex"),
            (format!("{head}0000: -1\n"), "negative"),
            (format!("{head}0000: 12345\n"), "too many digits"),
            (format!("{head}0010: 1\n"), "address out of range"),
            (format!("{head}0004: 1\n0000: 1\n"), "address going back"),
            (format!("{head}0000: {}\n", "1 ".repeat(17)), "more words than declared"),
            (format!("{head}0000: 1\n{}\n", "x".repeat(MAX_LINE_LEN + 1)), "long line"),
            (format!("{head}width 4\n"), "header after data"),
            (format!("{head}words 99999999999999999999\n"), "absurd count"),
        ];
        for (text, why) in cases {
            assert!(parse(text.as_bytes()).is_err(), "accepted: {why}");
        }
        assert_eq!(parse(&vec![b' '; MAX_MAP_SIZE + 1]), Err(MapError::TooLarge));
        assert_eq!(parse(b"\xff\xfe"), Err(MapError::NotAMap));
    }

    #[test]
    fn file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("bw-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.map");
        let map = MemoryMap { data_bits: 8, words: (0..=255).collect() };
        save_path(&map, &path).unwrap();
        assert_eq!(load_path(&path).unwrap(), map);
        // Digital Works binary: same words (read back as 16-bit words).
        let dw = dir.join("dw.map");
        save_dw_path(&map, &dw).unwrap();
        let bytes = std::fs::read(&dw).unwrap();
        assert!(is_dw_map(&bytes));
        assert_eq!(bytes.len(), 8 + 4 * 256);
        assert_eq!(load_path(&dw).unwrap().words, map.words);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The official sample map is rewritten byte for byte.
    #[test]
    fn digital_works_sample_rewrites_identically() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dwm/low1.map");
        let bytes = std::fs::read(p).unwrap();
        let map = parse_dw(&bytes).unwrap();
        assert_eq!(to_dw_bytes(&map), bytes);
    }
}
