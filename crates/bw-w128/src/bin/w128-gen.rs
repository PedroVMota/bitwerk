//! Writes `examples/w128-computer.bitwerk` (the Andorinha W-128 board with
//! the demo program) and prints its statistics; with `--run` it also runs
//! the demo to HALT in the simulator and checks it against the reference
//! model.
//!
//! `cargo run --release -p bw-w128 --bin w128-gen [-- --run] [--out PATH]`

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let out =
        args.iter().position(|a| a == "--out").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| {
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/w128-computer.bitwerk").to_string()
        });
    let t0 = Instant::now();
    let text = match bw_w128::board::project_text() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let gen_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if let Err(e) = std::fs::write(&out, &text) {
        eprintln!("error: cannot write {out}: {e}");
        return ExitCode::FAILURE;
    }
    println!("wrote {out} ({} bytes) in {gen_ms:.0} ms", text.len());
    // `--dump-geometry FILE`: the root circuit's boxes and wire segments as
    // text (a layout debugging aid for external plotting).
    if let Some(path) = args.iter().position(|a| a == "--dump-geometry").and_then(|i| args.get(i + 1)) {
        match bw_io::load_from_bytes(text.as_bytes()) {
            Ok(p) => {
                if let Err(e) = std::fs::write(path, bw_w128::layout::geometry_text(&p.circuit)) {
                    eprintln!("error: cannot write {path}: {e}");
                    return ExitCode::FAILURE;
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    match bw_w128::report::report(text.as_bytes(), args.iter().any(|a| a == "--run")) {
        Ok(r) => {
            println!("{r}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
