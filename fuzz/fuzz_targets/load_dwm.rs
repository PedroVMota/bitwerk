//! Fuzzes the Digital Works importer with arbitrary bytes.
//!
//! Invariants under test: `dwm::import_project` / `dwm::import_macro` and
//! the template reader never panic; every accepted circuit passes the
//! model's invariant checks and round-trips through the project format.
//!
//! Run (needs nightly + cargo-fuzz):
//! `cargo +nightly fuzz run load_dwm fuzz/seeds/load_dwm`

#![no_main]
#![forbid(unsafe_code)]

use bw_io::dwm;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((project, report)) = dwm::import_project(data, "fuzz.dwm") {
        project.circuit.check().expect("imported circuit must be valid");
        let _ = report.to_text();
        let text = bw_io::save_to_string(&project).expect("imported circuit must serialize");
        bw_io::load_from_bytes(text.as_bytes()).expect("saved import must load");
    }
    if let Ok((file, _)) = dwm::import_macro(data, "fuzz.dwm") {
        let text = bw_io::bwmacro::save_to_string(&file.table, file.root).expect("macro must serialize");
        bw_io::bwmacro::load_from_bytes(text.as_bytes()).expect("saved macro must load");
    }
    let _ = dwm::read_template(data);
    let _ = bw_io::memmap::parse(data);
});
