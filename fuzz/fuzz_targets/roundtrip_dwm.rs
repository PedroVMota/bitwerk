//! Fuzzes the Digital Works writer and exporter.
//!
//! Invariants under test:
//! * any stream the reader accepts is written back by the writer, and the
//!   rewritten stream reads back as the same object graph;
//! * any circuit that imports (from a `.dwm`) or loads (from a bitwerk
//!   project) exports without panicking, and the export imports again;
//! * the export reports are well formed.
//!
//! Run (needs nightly + cargo-fuzz):
//! `cargo +nightly fuzz run roundtrip_dwm fuzz/seeds/roundtrip_dwm`

#![no_main]
#![forbid(unsafe_code)]

use bw_io::dwm;
use libfuzzer_sys::fuzz_target;

fn export(project: &bw_io::Project) {
    match dwm::export_project(project, "fuzz.dwm") {
        Ok((bytes, report)) => {
            let _ = report.to_text();
            dwm::import_project(&bytes, "fuzz.dwm").expect("an export must import again");
        }
        // Macros are copied into every instance: a deep hierarchy can be
        // too large for the format; nothing else may fail.
        Err(e) => assert!(e.to_string().contains("too large"), "export failed: {e}"),
    }
    for (id, _) in project.circuit.macros().iter() {
        if let Ok((bytes, _)) = dwm::export_macro(project.circuit.macros(), id, "m.dwm") {
            dwm::import_macro(&bytes, "m.dwm").expect("a macro export must import again");
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(g) = dwm::reader::parse(data) {
        let bytes = dwm::writer::write(&g).expect("an accepted stream must be writable");
        let back = dwm::reader::parse(&bytes).expect("a written stream must read back");
        assert_eq!(back.objects, g.objects, "rewrite changed the object graph");
        assert_eq!(back.header, g.header, "rewrite changed the header");
    }
    if let Ok((project, _)) = dwm::import_project(data, "fuzz.dwm") {
        export(&project);
    }
    if let Ok(project) = bw_io::load_from_bytes(data) {
        export(&project);
    }
});
