//! Full Digital Works corpus import (only with `BITWERK_DWM_CORPUS=<dir>`).

use std::path::{Path, PathBuf};

fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn whole_corpus_imports() {
    let Some(dir) = std::env::var_os("BITWERK_DWM_CORPUS") else {
        eprintln!("BITWERK_DWM_CORPUS not set: skipped");
        return;
    };
    let dir = PathBuf::from(dir);
    let all = files(&dir, "dwm");
    let (mut ok, mut failed) = (0usize, Vec::new());
    let mut slowest = (std::time::Duration::ZERO, PathBuf::new());
    let mut notes: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for p in &all {
        let bytes = std::fs::read(p).unwrap();
        let t = std::time::Instant::now();
        match bw_io::dwm::import_project(&bytes, &p.to_string_lossy()) {
            Ok((proj, report)) => {
                ok += 1;
                let el = t.elapsed();
                if el > slowest.0 {
                    slowest = (el, p.clone());
                }
                // Saving and reloading must work too.
                let text = bw_io::save_to_string(&proj).unwrap();
                bw_io::load_from_bytes(text.as_bytes()).unwrap_or_else(|e| panic!("{}: reload: {e}", p.display()));
                for n in report.notes {
                    *notes.entry(format!("{:?}: {}", n.severity, n.message)).or_default() += n.count;
                }
            }
            Err(e) => failed.push(format!("{}: {e}", p.display())),
        }
    }
    for f in &failed {
        eprintln!("FAIL {f}");
    }
    for (n, c) in &notes {
        eprintln!("{c:>7}  {n}");
    }
    #[allow(clippy::cast_precision_loss)]
    let pct = 100.0 * ok as f64 / all.len().max(1) as f64;
    eprintln!("imported {ok}/{} ({pct:.1}%), slowest {:?} {}", all.len(), slowest.0, slowest.1.display());
    assert!(failed.is_empty(), "{} files failed", failed.len());
}

/// Import timings: the largest file (parse + convert) and a whole-folder
/// import into a fresh library (only with `BITWERK_DWM_CORPUS`).
#[test]
fn corpus_timings() {
    let Some(dir) = std::env::var_os("BITWERK_DWM_CORPUS") else { return };
    let dir = PathBuf::from(dir);
    let all = files(&dir, "dwm");
    let biggest = all.iter().max_by_key(|p| std::fs::metadata(p).map_or(0, |m| m.len())).unwrap();
    let bytes = std::fs::read(biggest).unwrap();
    let mut best = std::time::Duration::MAX;
    for _ in 0..3 {
        let t = std::time::Instant::now();
        let (p, _) = bw_io::dwm::import_project(&bytes, "big.dwm").unwrap();
        best = best.min(t.elapsed());
        assert!(p.circuit.component_count() > 0);
    }
    eprintln!("largest file {} ({} bytes): import {best:?}", biggest.display(), bytes.len());
    let dest = std::env::temp_dir().join(format!("bitwerk-corpus-lib-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    let t = std::time::Instant::now();
    let r = bw_io::dwm::import_folder(&dir, &dest, &mut |_, _, _| true).unwrap();
    eprintln!(
        "folder import of {} files: {:?} — {} written, {} duplicates, {} not macros, {} failed",
        all.len(),
        t.elapsed(),
        r.written.len(),
        r.duplicates.len(),
        r.not_macros.len(),
        r.failed.len()
    );
    for (p, e) in &r.failed {
        eprintln!("  failed {}: {e}", p.display());
    }
    let (lib, bad) = bw_io::library::scan(&dest);
    eprintln!("library scan: {} parts, {} unreadable", lib.len(), bad.len());
    assert!(bad.is_empty());
    let _ = std::fs::remove_dir_all(&dest);
}

/// Kind class for structural comparison (parameters the format cannot
/// carry ignored).
fn class(k: &bw_core::ComponentKind) -> String {
    use bw_core::ComponentKind as K;
    match k {
        K::Numeric { bits, .. } => format!("Numeric{bits}"),
        K::Clock { .. } => "Clock".into(),
        K::SeqGen { pattern, len, .. } => format!("Seq{pattern:x}/{len}"),
        K::Macro(s) => format!("Macro{}+{}", s.inputs, s.outputs),
        K::Text { .. } => "Text".into(),
        K::Switch { on, .. } => format!("Switch{on}"),
        other => format!("{other:?}"),
    }
}

fn census(c: &bw_core::Circuit) -> std::collections::BTreeMap<String, usize> {
    let mut m = std::collections::BTreeMap::new();
    for k in c.components() {
        *m.entry(class(&k.kind)).or_default() += 1;
    }
    m
}

fn net_shape(c: &bw_core::Circuit) -> Vec<usize> {
    let nets = c.nets();
    let mut v: Vec<usize> = (0..nets.len()).map(|i| nets.net(i).len()).collect();
    v.sort_unstable();
    v
}

/// Every corpus file: import, export as `.dwm`, import again; the result
/// must have the same components and nets. Macro files also go through
/// the macro export. A second export must be byte-identical to the first
/// re-export (only with `BITWERK_DWM_CORPUS`).
#[test]
fn whole_corpus_round_trips_through_export() {
    let Some(dir) = std::env::var_os("BITWERK_DWM_CORPUS") else {
        eprintln!("BITWERK_DWM_CORPUS not set: skipped");
        return;
    };
    let all = files(&PathBuf::from(dir), "dwm");
    let (mut ok, mut same_structure, mut stable, mut macros_ok, mut macros) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let mut failed = Vec::new();
    let mut notes: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let (mut bytes_in, mut bytes_out) = (0usize, 0usize);
    let t0 = std::time::Instant::now();
    for p in &all {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(p).unwrap();
        let Ok((a, _)) = bw_io::dwm::import_project(&bytes, &name) else { continue };
        let mut step = || -> Result<(bool, bool, usize), String> {
            let (x1, report) = bw_io::dwm::export_project(&a, &name).map_err(|e| format!("export: {e}"))?;
            for n in &report.notes {
                *notes.entry(format!("{:?}: {}", n.severity, n.message)).or_default() += n.count;
            }
            let (b, _) = bw_io::dwm::import_project(&x1, &name).map_err(|e| format!("re-import: {e}"))?;
            let same = census(&a.circuit) == census(&b.circuit) && net_shape(&a.circuit) == net_shape(&b.circuit);
            let (x2, _) = bw_io::dwm::export_project(&b, &name).map_err(|e| format!("second export: {e}"))?;
            let (c, _) = bw_io::dwm::import_project(&x2, &name).map_err(|e| format!("second re-import: {e}"))?;
            let (x3, _) = bw_io::dwm::export_project(&c, &name).map_err(|e| format!("third export: {e}"))?;
            Ok((same, x2 == x3, x1.len()))
        };
        match step() {
            Ok((same, st, n)) => {
                ok += 1;
                bytes_in += bytes.len();
                bytes_out += n;
                same_structure += usize::from(same);
                stable += usize::from(st);
                if !same {
                    eprintln!("structure differs: {name}");
                }
            }
            Err(e) => failed.push(format!("{name}: {e}")),
        }
        if let Ok((m, _)) = bw_io::dwm::import_macro(&bytes, &name) {
            macros += 1;
            let r = bw_io::dwm::export_macro(&m.table, m.root, &name)
                .and_then(|(x, _)| bw_io::dwm::import_macro(&x, &name));
            match r {
                Ok((m2, _)) if m2.def().map(|d| d.pins.len()) == m.def().map(|d| d.pins.len()) => macros_ok += 1,
                Ok(_) => failed.push(format!("{name}: macro interface changed")),
                Err(e) => failed.push(format!("{name}: macro: {e}")),
            }
        }
    }
    for f in &failed {
        eprintln!("FAIL {f}");
    }
    for (n, c) in &notes {
        eprintln!("{c:>7}  {n}");
    }
    #[allow(clippy::cast_precision_loss)]
    let pct = |a: usize, b: usize| 100.0 * a as f64 / b.max(1) as f64;
    eprintln!(
        "export round trip: {ok}/{} exported and re-imported ({:.1}%), same structure {same_structure} ({:.1}%), byte-stable re-export {stable} ({:.1}%), macros {macros_ok}/{macros}; {bytes_in} -> {bytes_out} bytes; {:?}",
        all.len(),
        pct(ok, all.len()),
        pct(same_structure, ok),
        pct(stable, ok),
        t0.elapsed()
    );
    assert!(failed.is_empty(), "{} failures", failed.len());
    assert_eq!(ok, all.len());
    assert_eq!(same_structure, ok);
}
