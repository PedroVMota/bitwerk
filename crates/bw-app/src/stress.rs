//! Debug stress-test circuit generator (see [`bw_core::stress`]).

pub use bw_core::stress::generate;

#[cfg(test)]
mod tests {
    /// `cargo test --release -p bw-app -- --ignored --nocapture stress_sim_cost`
    #[test]
    #[ignore = "benchmark; run manually"]
    fn stress_sim_cost() {
        let c = super::generate(100_000).unwrap();
        let t = std::time::Instant::now();
        let net = bw_sim::Netlist::compile(&c).unwrap();
        let mut s = bw_sim::SimState::new(net, &c);
        s.reset().unwrap();
        println!("compile + initial settle: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        for _ in 0..200 {
            s.tick().unwrap();
        }
        println!("avg tick (with clock edges rippling): {:?}", t.elapsed() / 200);
    }
}
