//! Fuzzes the `.bwmacro` loader with arbitrary bytes.
//!
//! Invariant under test: `bwmacro::load_from_bytes` never panics, every
//! accepted file is a valid, acyclic table whose root uses every
//! definition, and it round-trips through the serializer.
//!
//! Run (needs nightly + cargo-fuzz):
//! `cargo +nightly fuzz run load_macro fuzz/seeds/load_macro`

#![no_main]
#![forbid(unsafe_code)]

use bw_io::bwmacro;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(file) = bwmacro::load_from_bytes(data) {
        assert!(file.def().is_some());
        assert_eq!(file.table.closure(file.root).len(), file.table.len());
        let text = bwmacro::save_to_string(&file.table, file.root).expect("accepted macro must serialize");
        let again = bwmacro::load_from_bytes(text.as_bytes()).expect("serialized macro must load");
        assert_eq!(again.hash(), file.hash());
    }
});
