//! Fuzzes the memory map (`.map`) parser with arbitrary bytes.
//!
//! Invariant under test: `memmap::parse` never panics, never allocates
//! beyond its limits, and anything it accepts round-trips through the
//! writer unchanged.
//!
//! Run (needs nightly + cargo-fuzz):
//! `cargo +nightly fuzz run load_memory_map fuzz/seeds/load_memory_map`

#![no_main]
#![forbid(unsafe_code)]

use bw_io::memmap;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(map) = memmap::parse(data) {
        // At most 2^14 words (the largest memory).
        assert!(map.words.len() <= 1 << 14);
        let text = memmap::to_string(&map);
        let again = memmap::parse(text.as_bytes()).expect("written map must parse");
        assert_eq!(again, map);
    }
});
