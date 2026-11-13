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
