//! Compact text encoding of memory contents inside project files.
//!
//! The words are written as lowercase hexadecimal tokens separated by single
//! spaces. A run of equal words is written once with a decimal repeat count
//! (`ff*12`), and trailing zero words are omitted, so an empty memory is the
//! empty string and a sparse one stays short. Example for
//! `[0x12, 0, 0, 0, 0xab]`: `"12 0*3 ab"`.
//!
//! Decoding treats the text as hostile input: its length, every count and
//! the total are bounded, values must fit the data width, and nothing is
//! allocated beyond the memory's size.

use thiserror::Error;

use crate::component::MAX_MEMORY_WORDS;

/// Longest accepted encoded string, in bytes (16K words of 4 hex digits
/// plus separators fit with room to spare).
pub const MAX_ENCODED_LEN: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MemEncError {
    #[error("memory contents too long")]
    TooLong,
    #[error("invalid memory word {0:?}")]
    BadToken(String),
    #[error("memory word {value:#x} does not fit in {bits} bits")]
    ValueTooWide { value: u32, bits: u8 },
    #[error("more words than the memory holds ({0})")]
    TooManyWords(usize),
}

/// Bit mask of a data width (1..=16 bits).
#[must_use]
pub const fn width_mask(bits: u8) -> u16 {
    if bits >= 16 { u16::MAX } else { (1u16 << bits) - 1 }
}

/// Encodes words (see the module docs).
#[must_use]
pub fn encode(words: &[u16]) -> String {
    use std::fmt::Write as _;
    let end = words.iter().rposition(|&w| w != 0).map_or(0, |i| i + 1);
    let mut out = String::new();
    let mut i = 0;
    while i < end {
        let w = words[i];
        let mut n = 1;
        while i + n < end && words[i + n] == w {
            n += 1;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(out, "{w:x}");
        if n > 1 {
            let _ = write!(out, "*{n}");
        }
        i += n;
    }
    out
}

/// Decodes `text` into exactly `len` words of `data_bits` bits (missing
/// trailing words are zero).
///
/// # Errors
/// Malformed tokens, values wider than `data_bits`, or more than `len` words.
pub fn decode(text: &str, len: usize, data_bits: u8) -> Result<Vec<u16>, MemEncError> {
    if text.len() > MAX_ENCODED_LEN {
        return Err(MemEncError::TooLong);
    }
    let len = len.min(MAX_MEMORY_WORDS);
    let mask = u32::from(width_mask(data_bits));
    let mut out = Vec::with_capacity(len);
    for tok in text.split_ascii_whitespace() {
        let bad = || MemEncError::BadToken(tok.chars().take(16).collect());
        let (val, count) = match tok.split_once('*') {
            Some((v, c)) => (v, c),
            None => (tok, "1"),
        };
        if val.is_empty() || val.len() > 4 || count.is_empty() || count.len() > 5 {
            return Err(bad());
        }
        if !val.bytes().all(|b| b.is_ascii_hexdigit()) || !count.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let value = u32::from_str_radix(val, 16).map_err(|_| bad())?;
        let count: usize = count.parse().map_err(|_| bad())?;
        if count == 0 {
            return Err(bad());
        }
        if value > mask {
            return Err(MemEncError::ValueTooWide { value, bits: data_bits });
        }
        if out.len() + count > len {
            return Err(MemEncError::TooManyWords(len));
        }
        #[allow(clippy::cast_possible_truncation, reason = "value <= mask <= 0xffff")]
        out.resize(out.len() + count, value as u16);
    }
    out.resize(len, 0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_compactness() {
        let mut w = vec![0u16; 256];
        w[0] = 0x12;
        w[4] = 0xab;
        w[5] = 0xab;
        w[6] = 0xab;
        let s = encode(&w);
        assert_eq!(s, "12 0*3 ab*3");
        assert_eq!(decode(&s, 256, 8).unwrap(), w);
        assert_eq!(encode(&[0; 16]), "");
        assert_eq!(decode("", 16, 4).unwrap(), vec![0; 16]);
    }

    #[test]
    fn rejects_hostile_input() {
        assert!(matches!(decode("1ff", 16, 8), Err(MemEncError::ValueTooWide { .. })));
        assert!(matches!(decode("1*17", 16, 8), Err(MemEncError::TooManyWords(16))));
        assert!(decode("1*0", 16, 8).is_err());
        assert!(decode("-1", 16, 8).is_err());
        assert!(decode("+1", 16, 8).is_err());
        assert!(decode("12345", 16, 16).is_err());
        assert!(decode("1*99999999999", 16, 8).is_err());
        assert!(decode("zz", 16, 8).is_err());
        assert!(decode("1**2", 16, 8).is_err());
        assert!(matches!(decode(&"0 ".repeat(200_000), 16, 8), Err(MemEncError::TooLong)));
    }
}
