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

    /// `cargo test --release -p bw-app -- --ignored --nocapture stress_macros_cost`
    #[test]
    #[ignore = "benchmark; run manually"]
    #[allow(clippy::cast_precision_loss, clippy::many_single_char_names, clippy::too_many_lines)]
    fn stress_macros_cost() {
        use std::sync::Arc;
        let t = std::time::Instant::now();
        let mut c = bw_io::library::stress_macros(2_500).unwrap();
        println!("generate 2500 cells: {:?}", t.elapsed());
        let mut cache = bw_sim::FlattenCache::default();
        let t = std::time::Instant::now();
        let (net, s) = bw_sim::Netlist::compile_cached(&c, &mut cache).unwrap();
        println!(
            "cold compile: {:?} (flatten {:?}, netlist {:?}); {} instances, {} primitives, {} nets, {} templates built",
            t.elapsed(),
            s.flatten,
            s.netlist,
            s.instances,
            s.primitives,
            s.nets,
            s.templates_built
        );
        let t = std::time::Instant::now();
        let mut sim = bw_sim::SimState::new(net, &c);
        sim.reset().unwrap();
        println!("initial settle: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        for _ in 0..200 {
            sim.tick().unwrap();
        }
        println!("avg tick: {:?}", t.elapsed() / 200);
        let (_, s) = bw_sim::Netlist::compile_cached(&c, &mut cache).unwrap();
        println!(
            "warm recompile (root edit): {:?} ({} templates reused, 0 built: {})",
            s.total(),
            s.templates_reused,
            s.templates_built
        );
        // Edit the innermost definition (half adder, 20 000 instances):
        // UI-side model apply, then the recompile (one template rebuilt).
        let ha = c.macros().iter().find(|(_, e)| e.def.name == "Half adder").map(|(id, _)| id).unwrap();
        let mut d = (**c.macros().def(ha).unwrap()).clone();
        let g = d.body.components().find(|k| matches!(k.kind, bw_core::ComponentKind::Gate { .. })).unwrap().id;
        let p = d.body.component(g).unwrap().pos;
        d.body.move_component(g, p.offset(0, 1)).unwrap();
        let t = std::time::Instant::now();
        c.apply(&bw_core::Edit::DefineMacro { id: ha, def: Arc::new(d) }).unwrap();
        println!("UI model: redefine the half adder ({} instances): {:?}", c.total_instances(ha), t.elapsed());
        let (_, s) = bw_sim::Netlist::compile_cached(&c, &mut cache).unwrap();
        println!("recompile after the edit: {:?} ({} templates rebuilt)", s.total(), s.templates_built);
        // Redefine the cell with a changed interface (2 500 root instances
        // re-shaped and re-wired) and patch a scene.
        let cell = c.macros().iter().find(|(_, e)| e.def.name == "Cell").map(|(id, _)| id).unwrap();
        let mut d = (**c.macros().def(cell).unwrap()).clone();
        d.body
            .add_component(
                bw_core::ComponentKind::Tag { dir: bw_core::TagDir::Out, tristate: false },
                bw_core::GridPos::new(80, 40),
                bw_core::Rotation::R0,
            )
            .unwrap();
        d.sync_pins();
        let mut scene = bw_render::Scene::build(&c, 1);
        let t = std::time::Instant::now();
        let a = c.apply(&bw_core::Edit::DefineMacro { id: cell, def: Arc::new(d) }).unwrap();
        let model = t.elapsed();
        scene.apply(&c, &a, 2);
        println!(
            "UI: redefine Cell with a new pin ({} instances re-shaped): model {:?}, model + scene {:?}",
            a.components.len(),
            model,
            t.elapsed()
        );
        let (_, s) = bw_sim::Netlist::compile_cached(&c, &mut cache).unwrap();
        println!("recompile after the interface change: {:?}", s.total());
    }
}
