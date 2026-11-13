//! Digital Works export: structure of export → import round trips.

#![allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::many_single_char_names,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bw_core::{Circuit, ComponentKind};
use bw_io::dwm;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dwm")
}

fn fixtures() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "dwm"))
        .collect();
    v.sort();
    v
}

fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "bitwerk"))
        .collect();
    v.sort();
    v
}

/// Kind class of a component for comparisons (parameters that the
/// format cannot carry are ignored).
fn class(k: &ComponentKind) -> String {
    match k {
        ComponentKind::Numeric { bits, .. } => format!("Numeric{bits}"),
        ComponentKind::Clock { .. } => "Clock".into(),
        ComponentKind::SeqGen { pattern, len, .. } => format!("Seq{pattern:x}/{len}"),
        ComponentKind::Macro(s) => format!("Macro{}+{}", s.inputs, s.outputs),
        ComponentKind::Text { .. } => "Text".into(),
        ComponentKind::Switch { on, .. } => format!("Switch{on}"),
        other => format!("{other:?}"),
    }
}

fn census(c: &Circuit) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for k in c.components() {
        *m.entry(class(&k.kind)).or_default() += 1;
    }
    m
}

/// Net sizes (pins per net), sorted: a structural fingerprint independent
/// of ids and geometry.
fn net_shape(c: &Circuit) -> Vec<usize> {
    let nets = c.nets();
    let mut v: Vec<usize> = (0..nets.len()).map(|i| nets.net(i).len()).collect();
    v.sort_unstable();
    v
}

fn roundtrip(name: &str, p: &bw_io::Project) -> (bw_io::Project, Vec<u8>, dwm::ExportReport) {
    let (bytes, report) =
        dwm::export_project(p, &format!("{name}.dwm")).unwrap_or_else(|e| panic!("{name}: export: {e}"));
    let (back, _) =
        dwm::import_project(&bytes, &format!("{name}.dwm")).unwrap_or_else(|e| panic!("{name}: re-import: {e}"));
    (back, bytes, report)
}

#[test]
fn fixtures_round_trip_structurally() {
    for path in fixtures() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let (a, _) = dwm::import_project(&std::fs::read(&path).unwrap(), &name).unwrap();
        let (b, bytes, report) = roundtrip(&name, &a);
        eprintln!("{name}: {} bytes\n{}", bytes.len(), report.to_text());
        assert_eq!(census(&a.circuit), census(&b.circuit), "{name}: components");
        assert_eq!(net_shape(&a.circuit), net_shape(&b.circuit), "{name}: nets");
        // Definitions that differed only in what the format cannot carry
        // (pin names, datasheet text) merge on import.
        assert!(b.circuit.macros().len() <= a.circuit.macros().len(), "{name}: macro definitions");
        assert!(!b.circuit.macros().is_empty() || a.circuit.macros().is_empty(), "{name}: macro definitions");
    }
}

#[test]
fn examples_export_and_reimport() {
    for path in examples() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let a = bw_io::load_from_path(&path).unwrap();
        let (b, bytes, report) = roundtrip(&name, &a);
        eprintln!("{name}: {} bytes\n{}", bytes.len(), report.to_text());
        eprintln!("  before {:?}\n  after  {:?}", census(&a.circuit), census(&b.circuit));
        assert!(b.circuit.component_count() >= a.circuit.component_count() / 2, "{name}");
    }
}

/// Every export (fixtures re-exported, examples, the fixture macros as
/// macros) also parses with the independent Python reference parser
/// `tools/dwm_parse.py` (skipped if `python3` is not installed).
#[test]
fn python_reference_parser_reads_every_export() {
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dwm-export-python");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let mut n = 0;
    for path in fixtures() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let (a, _) = dwm::import_project(&bytes, &name).unwrap();
        let (x, _) = dwm::export_project(&a, &name).unwrap();
        std::fs::write(out.join(format!("project-{name}")), x).unwrap();
        n += 1;
        if let Ok((m, _)) = dwm::import_macro(&bytes, &name) {
            let (x, _) = dwm::export_macro(&m.table, m.root, &name).unwrap();
            std::fs::write(out.join(format!("macro-{name}")), x).unwrap();
            n += 1;
        }
    }
    for path in examples() {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let a = bw_io::load_from_path(&path).unwrap();
        let (x, _) = dwm::export_project(&a, &name).unwrap();
        std::fs::write(out.join(format!("example-{name}.dwm")), x).unwrap();
        n += 1;
    }
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/dwm_parse.py");
    let Ok(res) = std::process::Command::new("python3").arg("-I").arg(&script).arg(&out).output() else {
        eprintln!("python3 not available: skipped");
        return;
    };
    let text = String::from_utf8_lossy(&res.stdout);
    eprintln!("{text}");
    assert!(res.status.success(), "dwm_parse.py failed:\n{text}\n{}", String::from_utf8_lossy(&res.stderr));
    assert!(text.contains(&format!("files OK: {n}  failed: 0")), "{text}");
}

#[path = "common/randcirc.rs"]
mod randcirc;

/// The exporter never fails or panics on circuits the model can hold
/// (every kind, rotation, wide gates, every flip-flop variant, memories,
/// text, nested macros, DIL packages), and its output always re-imports.
#[test]
fn random_circuits_export_and_reimport() {
    let mut defs = 0;
    for seed in 0..120u64 {
        let o = randcirc::Opts { parts: 10 + (seed as usize % 30), rotate: seed % 3 != 0, exact: false, body: false };
        let c = randcirc::random_project(seed, o);
        defs += c.macros().len();
        let p = bw_io::Project { name: format!("random {seed}"), circuit: c };
        let (bytes, report) = dwm::export_project(&p, "r.dwm").unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let (b, _) = dwm::import_project(&bytes, "r.dwm").unwrap_or_else(|e| panic!("seed {seed}: re-import: {e}"));
        assert!(b.circuit.component_count() > 0, "seed {seed}");
        assert_eq!(report.bytes, bytes.len());
        for (id, _) in p.circuit.macros().iter() {
            let (mb, _) =
                dwm::export_macro(p.circuit.macros(), id, "m.dwm").unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            dwm::import_macro(&mb, "m.dwm").unwrap_or_else(|e| panic!("seed {seed}: macro re-import: {e}"));
        }
    }
    assert!(defs > 20, "the generator makes macros ({defs})");
}

/// The stress generator's large circuits export (and quickly).
#[test]
fn stress_circuit_exports() {
    let c = bw_core::stress::generate(20_000).unwrap();
    let p = bw_io::Project { name: "stress".into(), circuit: c };
    let t = std::time::Instant::now();
    let (bytes, report) = dwm::export_project(&p, "stress.dwm").unwrap();
    eprintln!(
        "stress: {} components -> {} bytes, {} objects in {:?}",
        p.circuit.component_count(),
        bytes.len(),
        report.objects,
        t.elapsed()
    );
    let (b, _) = dwm::import_project(&bytes, "stress.dwm").unwrap();
    assert_eq!(b.circuit.component_count(), p.circuit.component_count());
    assert_eq!(net_shape(&b.circuit), net_shape(&p.circuit));
}

/// Re-export stability: once a circuit has been through Digital Works
/// (exported and imported), exporting it again gives the same file byte
/// for byte, and importing that gives the same structure.
#[test]
fn re_export_is_stable() {
    let mut sources: Vec<(String, bw_io::Project)> = Vec::new();
    for path in fixtures() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        sources.push((name.clone(), dwm::import_project(&std::fs::read(&path).unwrap(), &name).unwrap().0));
    }
    for path in examples() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        sources.push((name, bw_io::load_from_path(&path).unwrap()));
    }
    let (mut same, mut total) = (0, 0);
    for (name, p) in sources {
        let (b, x1, _) = roundtrip(&name, &p);
        let (c, x2, _) = roundtrip(&name, &b);
        let (_, x3, _) = roundtrip(&name, &c);
        assert_eq!(census(&b.circuit), census(&c.circuit), "{name}: components");
        assert_eq!(net_shape(&b.circuit), net_shape(&c.circuit), "{name}: nets");
        total += 1;
        if x2 == x3 {
            same += 1;
        } else {
            eprintln!("{name}: second and third export differ ({} / {} / {} bytes)", x1.len(), x2.len(), x3.len());
        }
    }
    eprintln!("byte-identical re-exports: {same}/{total}");
    assert_eq!(same, total);
}
