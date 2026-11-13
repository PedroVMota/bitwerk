//! Fuzzes the project loader with arbitrary bytes.
//!
//! Invariant under test: `load_from_bytes` never panics, never aborts on
//! allocation, and anything it accepts round-trips through the serializer.
//!
//! Run (needs nightly + cargo-fuzz):
//! `cargo +nightly fuzz run load_project fuzz/seeds/load_project`

#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(project) = bw_io::load_from_bytes(data) {
        let text = bw_io::save_to_string(&project).expect("accepted project must serialize");
        let again = bw_io::load_from_bytes(text.as_bytes()).expect("serialized project must load");
        assert_eq!(again.circuit.component_count(), project.circuit.component_count());
        assert_eq!(again.circuit.segment_count(), project.circuit.segment_count());
        assert_eq!(again.circuit.node_count(), project.circuit.node_count());
    }
});
