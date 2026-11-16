//! Digital Works export: structure of export → import round trips.

#![allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
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

/// Centres of the gates Digital Works draws itself (`x`, `y`, no stored
/// drawing), sorted.
fn native_gates(bytes: &[u8]) -> (Vec<(i64, i64)>, usize) {
    use bw_io::dwm::reader::{Cls, F};
    let g = dwm::reader::parse(bytes).unwrap();
    let mut native = Vec::new();
    let mut drawn = 0;
    for o in &g.objects {
        if !matches!(o.cls(), Cls::Gate(..)) {
            continue;
        }
        if o.bool(F::HasGraphics) == Some(false) {
            native.push((o.f32(F::X).unwrap() as i64, o.f32(F::Y).unwrap() as i64));
        } else {
            drawn += 1;
        }
    }
    native.sort_unstable();
    (native, drawn)
}

/// Unrotated gates are written as Digital Works' own gates (centre only,
/// its standard pin positions), as Digital Works writes them itself: a
/// gate with a stored drawing and pins at bitwerk's spacing does not keep
/// its wires attached once it is opened in Digital Works.
#[test]
fn unrotated_gates_are_native_digital_works_gates() {
    for path in fixtures() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let (a, _) = dwm::import_project(&bytes, &name).unwrap();
        let (x, _) = dwm::export_project(&a, &name).unwrap();
        let (src, src_drawn) = native_gates(&bytes);
        let (out, out_drawn) = native_gates(&x);
        // Positions differ (written at three times the grid, Digital
        // Works' gate centred on ours); every gate stays native.
        assert_eq!(src.len(), out.len(), "{name}: native gates");
        assert_eq!(src_drawn, out_drawn, "{name}: gates with a stored drawing");
    }
}

/// Pin positions (pixels from the centre, pin kind) of every device
/// Digital Works draws itself, in its pin order, as in every real file
/// (`tests/fixtures/dwm` and the Digital Works package).
fn native_layout(class: &str) -> Option<&'static [(f32, f32, i32)]> {
    Some(match class {
        "T2AndGate" | "T2NandGate" | "T2OrGate" | "T2NorGate" | "T2XorGate" | "T2XnorGate" => {
            &[(30.0, 0.0, 2), (-30.0, 10.0, 1), (-30.0, -10.0, 1)]
        }
        "T3AndGate" | "T3NandGate" | "T3OrGate" | "T3NorGate" => {
            &[(30.0, 0.0, 2), (-30.0, 10.0, 1), (-30.0, 0.0, 1), (-30.0, -10.0, 1)]
        }
        "T4AndGate" | "T4NandGate" | "T4OrGate" | "T4NorGate" => {
            &[(30.0, 0.0, 2), (-30.0, 15.0, 1), (-30.0, 5.0, 1), (-30.0, -5.0, 1), (-30.0, -15.0, 1)]
        }
        "TNotGate" => &[(30.0, 0.0, 2), (-20.0, 0.0, 1)],
        "TTriState" => &[(-20.0, 0.0, 1), (20.0, 0.0, 3), (0.0, 10.0, 1)],
        "TBitGenerator" | "TClockDevice" => &[(15.0, 0.0, 2)],
        "TSupply" => &[(10.0, 0.0, 2)],
        "TGround" => &[(0.0, -10.0, 2)],
        "TInteractiveInput" => &[(0.0, 0.0, 2)],
        "TInteractiveSwitch" => &[(0.0, -15.0, 3), (0.0, 15.0, 3)],
        "TLEDDevice" => &[(0.0, 0.0, 1)],
        "TLEDSegmentDevice" => &[
            (-15.0, 50.0, 1),
            (-10.0, 50.0, 1),
            (-5.0, 50.0, 1),
            (0.0, 50.0, 1),
            (5.0, 50.0, 1),
            (10.0, 50.0, 1),
            (15.0, 50.0, 1),
        ],
        "TDFlipFlop" => {
            &[(30.0, -10.0, 2), (30.0, 10.0, 2), (-30.0, -10.0, 1), (-30.0, 0.0, 1), (-20.0, 20.0, 1), (-10.0, 20.0, 1)]
        }
        "TJKFlipFlop" => &[
            (30.0, -10.0, 2),
            (30.0, 10.0, 2),
            (-30.0, -10.0, 1),
            (-30.0, 0.0, 1),
            (-20.0, 20.0, 1),
            (-10.0, 20.0, 1),
            (-30.0, 10.0, 1),
        ],
        "TRSFlipFlop" => &[(30.0, -10.0, 2), (30.0, 10.0, 2), (-30.0, -10.0, 1), (-30.0, 10.0, 1)],
        _ => return None,
    })
}

/// Checks a written file against what Digital Works itself writes, so
/// that every wire stays attached when the file is opened there:
///
/// * devices Digital Works always draws itself are stored without a
///   drawing, with their pins exactly at its standard positions and kinds
///   (otherwise it draws the device in one place and the wire ends sit in
///   another);
/// * only macros, tags, memories, numeric outputs and (rotated) gates
///   carry a drawing;
/// * every wire has two ends, each a pin, macro pin or solder that refers
///   back to the wire, and the wire's first / last point lies on that
///   pin's centre or the solder;
/// * every pin's wire and every solder's three wires refer back.
fn check_digital_works_conformance(name: &str, bytes: &[u8]) {
    use bw_io::dwm::reader::{Cls, F, Obj};
    let g = dwm::reader::parse(bytes).unwrap();
    let centre = |r: u32| -> (f32, f32) {
        let o = g.get(r).unwrap();
        if o.cls() == Cls::Solder {
            return (o.f32(F::X).unwrap(), o.f32(F::Y).unwrap());
        }
        let pts = g.points(o.r(F::Points));
        assert!(!pts.is_empty(), "{name}: pin #{r} without outline");
        let n = pts.len() as f32;
        (pts.iter().map(|p| p.0).sum::<f32>() / n, pts.iter().map(|p| p.1).sum::<f32>() / n)
    };
    let near = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() <= 0.5 && (a.1 - b.1).abs() <= 0.5;
    for (i, o) in g.objects.iter().enumerate() {
        let i = i as u32;
        let Obj::Rec(cls, _) = o else { continue };
        let class = cls.name();
        if let Some(gfx) = o.bool(F::HasGraphics) {
            let pins = g.items(o.r(F::Pins));
            if let Some(layout) = native_layout(class) {
                let rotatable_gate = matches!(cls, Cls::Gate(..));
                if gfx {
                    assert!(rotatable_gate, "{name}: {class} #{i} stored with a drawing");
                    continue;
                }
                let at = (o.f32(F::X).unwrap(), o.f32(F::Y).unwrap());
                assert_eq!(pins.len(), layout.len(), "{name}: {class} #{i} pin count");
                for (k, (&p, &(dx, dy, kind))) in pins.iter().zip(layout).enumerate() {
                    let c = centre(p);
                    assert!(
                        near(c, (at.0 + dx, at.1 + dy)),
                        "{name}: {class} #{i} at {at:?}: pin {k} at {c:?}, Digital Works puts it at ({dx}, {dy})"
                    );
                    assert_eq!(g.get(p).unwrap().int(F::PinKind), Some(kind), "{name}: {class} #{i} pin {k} kind");
                }
            } else {
                assert!(
                    matches!(cls, Cls::Macro | Cls::Tag | Cls::Memory | Cls::Numeric) == gfx,
                    "{name}: {class} #{i} has_graphics = {gfx}"
                );
            }
        }
        match cls {
            Cls::Wire => {
                let pts = g.points(o.r(F::Points));
                assert!(!pts.is_empty(), "{name}: wire #{i} without points");
                for (f, pt) in [(F::End1, pts[0]), (F::End2, pts[pts.len() - 1])] {
                    let e = o.r(f).unwrap_or_else(|| panic!("{name}: wire #{i} with a loose end"));
                    let eo = g.get(e).unwrap();
                    let back = match eo.cls() {
                        Cls::Pin | Cls::MacroPin => eo.r(F::Wire) == Some(i),
                        Cls::Solder => [F::Wire1, F::Wire2, F::Wire3].iter().any(|&w| eo.r(w) == Some(i)),
                        other => panic!("{name}: wire #{i} ends on a {}", other.name()),
                    };
                    assert!(back, "{name}: wire #{i}: its end #{e} does not refer back to it");
                    assert!(
                        near(centre(e), pt),
                        "{name}: wire #{i} ends at {pt:?}, its pin / solder is at {:?}",
                        centre(e)
                    );
                }
            }
            Cls::Pin | Cls::MacroPin => {
                if let Some(w) = o.r(F::Wire) {
                    let wo = g.get(w).unwrap();
                    assert!(
                        wo.cls() == Cls::Wire && (wo.r(F::End1) == Some(i) || wo.r(F::End2) == Some(i)),
                        "{name}: pin #{i}: its wire #{w} does not end on it"
                    );
                }
            }
            Cls::Solder => {
                for f in [F::Wire1, F::Wire2, F::Wire3] {
                    let w = o.r(f).unwrap_or_else(|| panic!("{name}: solder #{i} with fewer than 3 wires"));
                    let wo = g.get(w).unwrap();
                    assert!(
                        wo.r(F::End1) == Some(i) || wo.r(F::End2) == Some(i),
                        "{name}: solder #{i}: wire #{w} does not end on it"
                    );
                }
            }
            _ => {}
        }
    }
}

/// Every export (real files re-exported, their macros, the examples and
/// random circuits with every component kind, rotations, wide gates,
/// flip-flop variants, memories and nested macros) follows the rules
/// above.
#[test]
fn every_export_conforms_to_digital_works() {
    // The real files themselves pass the check (it encodes their rules).
    for path in fixtures() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        check_digital_works_conformance(&format!("original {name}"), &std::fs::read(&path).unwrap());
    }
    for path in fixtures() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        let (a, _) = dwm::import_project(&bytes, &name).unwrap();
        check_digital_works_conformance(&name, &dwm::export_project(&a, &name).unwrap().0);
        if let Ok((m, _)) = dwm::import_macro(&bytes, &name) {
            check_digital_works_conformance(&name, &dwm::export_macro(&m.table, m.root, &name).unwrap().0);
        }
    }
    for path in examples() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let a = bw_io::load_from_path(&path).unwrap();
        check_digital_works_conformance(&name, &dwm::export_project(&a, &name).unwrap().0);
    }
    for seed in 0..200u64 {
        let o = randcirc::Opts { parts: 10 + (seed as usize % 30), rotate: seed % 3 != 0, exact: false, body: false };
        let c = randcirc::random_project(seed, o);
        let p = bw_io::Project { name: format!("random {seed}"), circuit: c };
        let name = format!("random {seed}");
        check_digital_works_conformance(&name, &dwm::export_project(&p, "r.dwm").unwrap().0);
        for (id, _) in p.circuit.macros().iter() {
            check_digital_works_conformance(&name, &dwm::export_macro(p.circuit.macros(), id, "m.dwm").unwrap().0);
        }
    }
}
