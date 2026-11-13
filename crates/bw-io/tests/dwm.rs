//! Digital Works import on the official sample fixtures (see
//! `tests/fixtures/dwm/NOTICE`): parsing, conversion, deduplication,
//! templates, memory maps, folder import and hostile input.

#![allow(clippy::unwrap_used, clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

use bw_core::{ComponentKind, Package};
use bw_io::dwm::{self, Severity, reader};
use bw_io::memmap;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dwm")
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(name)).unwrap()
}

fn all(ext: &str) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir())
        .unwrap()
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == ext)
                .then(|| (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&p).unwrap()))
        })
        .collect();
    v.sort();
    v
}

#[test]
fn every_fixture_parses_and_imports() {
    let files = all("dwm");
    assert!(files.len() >= 15, "{} fixtures", files.len());
    let mut versions = std::collections::BTreeSet::new();
    let mut classes = std::collections::BTreeSet::new();
    for (name, bytes) in &files {
        let g = reader::parse(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        versions.insert(g.header.version);
        for o in &g.objects {
            classes.insert(format!("{:?}", o.cls()));
        }
        let (p, report) = dwm::import_project(bytes, name).unwrap_or_else(|e| panic!("{name}: {e}"));
        p.circuit.check().unwrap();
        assert_eq!(report.source, *name);
        let text = report.to_text();
        assert!(text.contains("Imported:"), "{text}");
        // Saving and loading give the same circuit.
        let saved = bw_io::save_to_string(&p).unwrap();
        let q = bw_io::load_from_bytes(saved.as_bytes()).unwrap();
        assert!(q.circuit.content_eq(&p.circuit), "{name}");
    }
    assert_eq!(versions.into_iter().collect::<Vec<_>>(), vec![20, 30, 34], "all three format versions");
    // Every device class of the corpus is covered by the fixtures.
    for c in [
        "Gate(0, 2)",
        "Gate(1, 2)",
        "Gate(2, 2)",
        "Gate(3, 4)",
        "Gate(4, 2)",
        "Gate(5, 2)",
        "Gate(6, 1)",
        "TriState",
        "DFlipFlop",
        "JkFlipFlop",
        "RsFlipFlop",
        "Memory",
        "Supply",
        "Ground",
        "InteractiveInput",
        "InteractiveSwitch",
        "Clock",
        "BitGenerator",
        "Led",
        "LedSegment",
        "Numeric",
        "Tag",
        "Macro",
        "Solder",
        "Wire",
        "GraphicText",
    ] {
        assert!(classes.contains(c), "no fixture has {c}: {classes:?}");
    }
}

#[test]
fn counter_report_and_model() {
    let (p, r) = dwm::import_project(&fixture("counter.dwm"), "counter.dwm").unwrap();
    assert_eq!(r.name, "Digital Counter Circuit");
    assert_eq!(r.version, 34);
    let count = |what: &str| r.counts.iter().find(|(w, _)| w == what).map_or(0, |(_, n)| *n);
    assert_eq!(count("7-segment displays"), 2);
    assert!(count("macro instances") >= 2 && count("wires") > 20 && count("junctions") > 0);
    assert!(r.macro_copies >= r.macro_defs && r.macro_defs >= 1);
    // Clock speed is not in the file: the report says what was assumed.
    assert!(r.notes.iter().any(|n| n.severity == Severity::Approximation && n.message.contains("clock speed")));
    let c = &p.circuit;
    let segs: Vec<_> = c.components().filter(|k| matches!(k.kind, ComponentKind::SevenSeg { .. })).collect();
    assert_eq!(segs.len(), 2);
    assert!(segs.iter().all(|s| s.kind == ComponentKind::SevenSeg { common_anode: true }), "common anode in the file");
    assert!(c.components().any(|k| matches!(k.kind, ComponentKind::Clock { half_period: 10 })));
    assert_eq!(c.eval_mode(), bw_core::EvalMode::Sequential);
}

/// Identical embedded copies become one definition; different ones stay
/// apart; instances reference them. The expected numbers were computed
/// independently by the research's Python reference (`dedup.py`).
#[test]
fn embedded_macros_are_deduplicated() {
    for (name, copies, defs) in [
        ("bus-contention.dwm", 9, 4),
        ("memory.dwm", 9, 4),
        ("74hc75.dwm", 4, 1),
        ("counter.dwm", 7, 7),
        ("rtx.dwm", 4, 3),
    ] {
        let (p, r) = dwm::import_project(&fixture(name), name).unwrap();
        assert_eq!((r.macro_copies, r.macro_defs), (copies, defs), "{name}");
        let table = p.circuit.macros();
        assert_eq!(table.len(), defs);
        let hashes: std::collections::BTreeSet<u64> = table.iter().map(|(_, e)| e.hash).collect();
        assert_eq!(hashes.len(), defs, "{name}: no two definitions with the same content");
        let instances: u64 = table.iter().map(|(id, _)| p.circuit.total_instances(id)).sum();
        assert_eq!(instances, copies as u64, "{name}: every copy is an instance");
        // Deterministic.
        let (q, _) = dwm::import_project(&fixture(name), name).unwrap();
        let h2: std::collections::BTreeSet<u64> = q.circuit.macros().iter().map(|(_, e)| e.hash).collect();
        assert_eq!(hashes, h2);
    }
}

#[test]
fn ics_import_as_dil_macros_with_datasheet_notes() {
    for (name, pins, part) in [
        ("74hc00.dwm", 14, "NAND"),
        ("74hc74.dwm", 14, "Flip"),
        ("74hc76.dwm", 16, "JK"),
        ("74hc125.dwm", 14, "Buffer"),
    ] {
        let (file, r) = dwm::import_macro(&fixture(name), name).unwrap();
        let def = file.def().unwrap();
        assert_eq!(def.package, Package::Dil(pins), "{name}");
        assert!(def.name.contains(part) || r.name.contains(part), "{name}: {}", def.name);
        assert!(def.datasheet.contains("Imported from Digital Works"));
        assert!(def.datasheet.contains(".pdf"), "{name}: the datasheet path is kept as a note: {}", def.datasheet);
        assert!(def.doc_link.is_none(), "never a link that could be opened");
        // Pins keep their DIL numbers.
        let shape = file.table.get(file.root).unwrap().shape;
        let mut nums: Vec<u8> = shape.dil_pins[..usize::from(shape.inputs + shape.outputs)].to_vec();
        nums.sort_unstable();
        nums.dedup();
        assert_eq!(nums.len(), usize::from(shape.inputs + shape.outputs));
        // Round trip through .bwmacro.
        let text = bw_io::bwmacro::save_to_string(&file.table, file.root).unwrap();
        assert_eq!(bw_io::bwmacro::load_from_bytes(text.as_bytes()).unwrap().hash(), file.hash());
    }
    // A plain circuit is not a macro.
    let e = dwm::import_macro(&fixture("xor-nand.dwm"), "xor-nand.dwm").unwrap_err();
    assert!(e.to_string().contains("not a macro"), "{e}");
}

#[test]
fn templates_are_dil_packages() {
    let t = dwm::read_template(&fixture("14-pin-dil.dwt")).unwrap();
    assert_eq!(t.pins.len(), 14);
    assert_eq!(t.package, Package::Dil(14));
    let relay = dwm::read_template(&fixture("relay.dwt")).unwrap();
    assert_eq!(relay.package, Package::Auto);
    assert!(dwm::import_project(&fixture("14-pin-dil.dwt"), "x.dwt").is_err(), "a template is not a circuit");
    assert!(dwm::read_template(&fixture("xor-nand.dwm")).is_err());
}

#[test]
fn digital_works_memory_maps() {
    let bytes = fixture("low1.map");
    assert!(memmap::is_dw_map(&bytes));
    let m = memmap::parse(&bytes).unwrap();
    assert_eq!(m.words.len(), 512);
    assert_eq!(m.data_bits, 16);
    assert_eq!(memmap::to_dw_bytes(&m), bytes, "writes back the same bytes");
    // Truncated, padded or lying files are refused.
    assert!(memmap::parse(&bytes[..bytes.len() - 1]).is_err());
    let mut longer = bytes.clone();
    longer.extend_from_slice(&[0; 4]);
    assert!(memmap::parse(&longer).is_err());
    let mut huge = bytes[..8].to_vec();
    huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(memmap::parse(&huge).is_err());
    // The memory sample embeds its contents.
    let (p, _) = dwm::import_project(&fixture("memory.dwm"), "memory.dwm").unwrap();
    let mem = p.circuit.components().find(|c| matches!(c.kind, ComponentKind::Memory { .. })).unwrap();
    assert!(p.circuit.memory(mem.id).unwrap().iter().any(|&w| w != 0), "contents imported");
}

#[test]
fn numeric_and_sequence_settings() {
    let (p, _) = dwm::import_project(&fixture("numbers.dwm"), "numbers.dwm").unwrap();
    let nums: Vec<_> = p.circuit.components().filter(|c| matches!(c.kind, ComponentKind::Numeric { .. })).collect();
    assert!(!nums.is_empty());
    for n in &nums {
        let ComponentKind::Numeric { bits, .. } = n.kind else { unreachable!() };
        assert_eq!(bits, 8);
    }
    assert!(nums.iter().any(|n| n.color.is_some()), "display colours imported");
    let (t, _) = dwm::import_project(&fixture("traffic-lights.dwm"), "traffic-lights.dwm").unwrap();
    let seqs: Vec<_> = t.circuit.components().filter(|c| matches!(c.kind, ComponentKind::SeqGen { .. })).collect();
    assert!(!seqs.is_empty());
    for s in seqs {
        let ComponentKind::SeqGen { len, half_period, .. } = s.kind else { unreachable!() };
        assert!(len >= 2 && half_period == 10);
    }
}

#[test]
fn folder_import_writes_deduplicated_parts() {
    let tmp = std::env::temp_dir().join(format!("bitwerk-dwm-folder-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let src = tmp.join("src");
    let dest = tmp.join("Parts/Imported");
    std::fs::create_dir_all(src.join("ICs")).unwrap();
    for f in ["74hc00.dwm", "74hc125.dwm"] {
        std::fs::copy(dir().join(f), src.join("ICs").join(f)).unwrap();
    }
    std::fs::copy(dir().join("74hc00.dwm"), src.join("copy-of-00.dwm")).unwrap();
    std::fs::copy(dir().join("xor-nand.dwm"), src.join("circuit.dwm")).unwrap();
    std::fs::write(src.join("broken.dwm"), b"\x04\xdd\xc4\x22\x00garbage").unwrap();
    let mut calls = 0;
    let r = dwm::import_folder(&src, &dest, &mut |_, _, _| {
        calls += 1;
        true
    })
    .unwrap();
    assert_eq!(r.written.len(), 2, "{r:?}");
    assert_eq!(r.duplicates.len(), 1);
    assert_eq!(r.not_macros.len(), 1);
    assert_eq!(r.failed.len(), 1);
    assert!(calls >= 5);
    assert!(r.written.iter().any(|p| p.starts_with(dest.join("ICs"))), "sub-folders kept");
    let (lib, bad) = bw_io::library::scan(&dest);
    assert!(bad.is_empty());
    assert_eq!(lib.len(), 2);
    // Importing again finds everything already there.
    let again = dwm::import_folder(&src, &dest, &mut |_, _, _| true).unwrap();
    assert!(again.written.is_empty() && again.duplicates.len() == 3);
    // Cancelling stops at once.
    let c = dwm::import_folder(&src, &dest, &mut |_, _, _| false).unwrap();
    assert!(c.cancelled && c.written.is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn hostile_streams_are_clean_errors() {
    let good = fixture("xor-nand.dwm");
    // Truncation at every length.
    for n in 0..good.len() {
        assert!(dwm::import_project(&good[..n], "t.dwm").is_err(), "prefix {n} accepted");
    }
    // Trailing bytes.
    let mut t = good.clone();
    t.push(0);
    assert!(dwm::import_project(&t, "t.dwm").is_err());
    // A future version.
    let mut v = good.clone();
    v[3] = 0x30;
    assert!(dwm::import_project(&v, "t.dwm").unwrap_err().to_string().contains("version"));
    // Unknown class name.
    let mut stream = vec![0x04, 0xdd, 0xc4, 0x14, 0x00, 0x02, 0x00, 0x06, 0x07];
    stream.extend_from_slice(b"TEvilXX");
    assert!(dwm::import_project(&stream, "t.dwm").unwrap_err().to_string().contains("unknown class"));
    // Deep nesting: a list containing a list containing a list … (no
    // stack overflow; the explicit stack and its limit catch it).
    let mut deep = vec![0x04, 0xdd, 0xc4, 0x14, 0x00];
    deep.extend_from_slice(&[0x02, 0x00, 0x06, 14]);
    deep.extend_from_slice(b"TSerializeList");
    deep.push(0x01);
    for _ in 0..300_000 {
        deep.extend_from_slice(&[0x02, 0x01, 0x02, 0x00, 0x01]);
    }
    let e = dwm::import_project(&deep, "t.dwm").unwrap_err().to_string();
    assert!(e.contains("nested") || e.contains("end of file"), "{e}");
    // A huge memory word count is refused before allocating.
    let mut words = vec![0x04, 0xdd, 0xc4, 0x14, 0x00];
    words.extend_from_slice(&[0x02, 0x00, 0x06, 13]);
    words.extend_from_slice(b"TMemoryDevice");
    words.extend_from_slice(&[0x08, 0x08, 0x08, 0x02, 0x0f, 0, 0, 0, 0, 0x02, 0x08, 0x02, 0x08, 0x08, 0x08]);
    words.extend_from_slice(&[0x04, 0xff, 0xff, 0xff, 0x7f]);
    assert!(dwm::import_project(&words, "t.dwm").is_err());
}

/// Deterministic mutation of the fixtures: thousands of corrupted files
/// (bytes flipped, values changed, chunks deleted or repeated) must never
/// panic; whatever is accepted is a valid circuit that saves and loads.
#[test]
fn mutated_fixtures_never_panic() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = |n: usize| -> usize {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        usize::try_from(seed % n.max(1) as u64).unwrap()
    };
    let files: Vec<Vec<u8>> = ["xor-nand.dwm", "74hc00.dwm", "ms-jk.dwm", "logic-history.dwm", "2-din-xnor.dwm"]
        .iter()
        .map(|f| fixture(f))
        .collect();
    let (mut total, mut accepted) = (0, 0);
    for base in &files {
        for _ in 0..800 {
            let mut m = base.clone();
            for _ in 0..=rnd(3) {
                let at = 5 + rnd(m.len().saturating_sub(5));
                let last = m.len() - 1;
                match rnd(6) {
                    0 => m[at.min(last)] ^= 1 << rnd(8),
                    1 => m[at.min(last)] = [0, 1, 2, 4, 6, 8, 9, 15, 0xff][rnd(9)],
                    2 => {
                        let n = rnd(16).min(m.len() - at.min(m.len()));
                        m.drain(at.min(m.len())..at.min(m.len()) + n);
                    }
                    3 => {
                        let n = rnd(32).min(m.len() - at.min(m.len()));
                        let chunk = m[at.min(m.len())..at.min(m.len()) + n].to_vec();
                        let to = 5 + rnd(m.len().saturating_sub(5));
                        m.splice(to..to, chunk);
                    }
                    4 => {
                        // An integer value somewhere in range of the tables.
                        let at = at.min(m.len().saturating_sub(2));
                        m[at] = 0x02;
                        m[at + 1] = u8::try_from(rnd(256)).unwrap();
                    }
                    _ => {
                        let at = at.min(m.len().saturating_sub(5));
                        m[at] = 0x04;
                        m[at + 1..at + 5].copy_from_slice(&u32::try_from(rnd(1 << 31)).unwrap().to_le_bytes());
                    }
                }
                if m.len() < 6 {
                    break;
                }
            }
            total += 1;
            if let Ok((p, r)) = dwm::import_project(&m, "m.dwm") {
                accepted += 1;
                p.circuit.check().unwrap();
                let _ = r.to_text();
                let text = bw_io::save_to_string(&p).unwrap();
                bw_io::load_from_bytes(text.as_bytes()).unwrap();
            }
            let _ = dwm::import_macro(&m, "m.dwm");
        }
    }
    eprintln!("{accepted} of {total} mutants accepted");
    assert!(total >= 4000 && accepted > 0 && accepted < total, "{accepted} of {total}");
}
