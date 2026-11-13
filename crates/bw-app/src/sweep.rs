//! Deterministic measurement mode (`bitwerk --stress --sweep`).
//!
//! Once the stress circuit is on screen, a scripted ~20 s camera path runs
//! (idle at full view, zoom in close, pan across, zoom back out), followed by
//! a burst of scripted edits (move / rotate / add a part / branch a wire off
//! a segment (split + junction) / drag a segment / move a bend / delete a
//! segment / delete a part). Every
//! frame's interval and CPU work are recorded; on exit a summary table is
//! printed (and optionally a per-frame CSV written with `--sweep-csv <path>`)
//! and the app quits. Nothing here depends on a human touching the window.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use bw_core::{ComponentId, ComponentKind, Edit, GridPos, PinRef, SegmentId, WireEnd};
use bw_render::Item;
use glam::Vec2;

use crate::app::BitwerkApp;

/// Number of scripted edits.
const EDITS: usize = 200;
/// Close zoom reached by the script (points per grid unit).
const ZOOM_CLOSE: f32 = 40.0;
const ZOOM_PAN: f32 = 8.0;
const ZOOM_EDIT: f32 = 6.0;
/// Frames recorded after the scene first appears, counted as "load".
const LOAD_TAIL_FRAMES: u32 = 30;
/// Nominal frame budget at 60 Hz.
const BUDGET_MS: f64 = 1000.0 / 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Load,
    IdleFit,
    ZoomIn,
    ZoomMid,
    Pan,
    ZoomOut,
    Edits,
    IdleEnd,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::IdleFit => "idle-fit",
            Self::ZoomIn => "zoom-in",
            Self::ZoomMid => "zoom-mid",
            Self::Pan => "pan",
            Self::ZoomOut => "zoom-out",
            Self::Edits => "edits",
            Self::IdleEnd => "idle-end",
        }
    }
}

struct FrameRec {
    phase: Phase,
    /// Interval since the previous frame started (ms).
    dt: f64,
    /// CPU work of the previous frame as reported by eframe (ms, excludes
    /// waiting for vsync).
    cpu: f64,
    /// Time spent in the canvas renderer's prepare (ms) last frame.
    prepare: f64,
    /// Instances drawn last frame.
    drawn: u32,
    /// Notable app events during the frame (e.g. a scene arrived).
    events: String,
    /// The previous pass (whose duration `dt` is) was actually painted and
    /// presented. Passes eframe runs without painting (occluded or minimized
    /// window) are excluded from the statistics and invalidate the run.
    presented: bool,
}

struct EditRec {
    revision: u64,
    at: Instant,
    frame: u64,
    visible: Option<(f64, u64)>,
}

pub struct Sweep {
    started: Instant,
    run_start: Option<Instant>,
    loaded_frames: u32,
    last_frame: Option<Instant>,
    frame_idx: u64,
    frames: Vec<FrameRec>,
    edits: Vec<EditRec>,
    targets: Vec<ComponentId>,
    added: Vec<ComponentId>,
    fit: Option<(Vec2, f32)>,
    csv: Option<PathBuf>,
    done: bool,
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

/// Exponential interpolation (zoom feels linear to the eye).
fn elerp(a: f32, b: f32, t: f32) -> f32 {
    (a.ln() + (b.ln() - a.ln()) * t.clamp(0.0, 1.0)).exp()
}

impl Sweep {
    pub fn new(csv: Option<PathBuf>) -> Self {
        Self {
            started: Instant::now(),
            run_start: None,
            loaded_frames: 0,
            last_frame: None,
            frame_idx: 0,
            frames: Vec::with_capacity(4096),
            edits: Vec::with_capacity(EDITS),
            targets: Vec::new(),
            added: Vec::new(),
            fit: None,
            csv,
            done: false,
        }
    }

    /// Called at the start of every frame (before the UI is built).
    /// Returns `true` when the sweep is finished and the app should quit.
    pub fn begin_frame(&mut self, app: &mut BitwerkApp, cpu_prev: Option<f32>, canvas: Vec2, presented: bool) -> bool {
        if self.done {
            return true;
        }
        let now = Instant::now();
        let dt = self.last_frame.map_or(0.0, |l| (now - l).as_secs_f64() * 1000.0);
        self.last_frame = Some(now);
        self.frame_idx += 1;

        let loaded = app.scene_ready() && app.scene_instance_count() > 0;
        let phase = if let Some(start) = self.run_start {
            let t = (now - start).as_secs_f32();
            let Some(p) = self.script(app, t, canvas) else {
                self.done = true;
                self.report();
                return true;
            };
            p
        } else {
            if loaded {
                self.loaded_frames += 1;
                if self.loaded_frames >= LOAD_TAIL_FRAMES {
                    self.run_start = Some(now);
                    self.fit = Some(fit(app, canvas));
                }
            }
            Phase::Load
        };
        let prof = app.frame_profile();
        self.frames.push(FrameRec {
            phase,
            dt,
            cpu: cpu_prev.map_or(0.0, |c| f64::from(c) * 1000.0),
            prepare: prof.0,
            drawn: prof.1,
            events: app.take_frame_events(),
            presented,
        });
        false
    }

    /// Called after the canvas has been handed to the renderer.
    pub fn end_frame(&mut self, app: &BitwerkApp) {
        let shown = app.displayed_revision();
        let now = Instant::now();
        for e in &mut self.edits {
            if e.visible.is_none() && shown >= e.revision {
                e.visible = Some(((now - e.at).as_secs_f64() * 1000.0, self.frame_idx - e.frame));
            }
        }
    }

    fn script(&mut self, app: &mut BitwerkApp, t: f32, canvas: Vec2) -> Option<Phase> {
        let (center, z_fit) = self.fit.unwrap_or((Vec2::ZERO, 0.2));
        let cam = &mut app.camera;
        let phase = match t {
            t if t < 2.0 => {
                cam.center = center;
                cam.zoom = z_fit;
                Phase::IdleFit
            }
            t if t < 5.0 => {
                cam.center = center;
                cam.zoom = elerp(z_fit, ZOOM_CLOSE, (t - 2.0) / 3.0);
                Phase::ZoomIn
            }
            t if t < 6.0 => {
                cam.center = center;
                cam.zoom = elerp(ZOOM_CLOSE, ZOOM_PAN, t - 5.0);
                Phase::ZoomMid
            }
            t if t < 10.0 => {
                let k = (t - 6.0) / 4.0;
                cam.zoom = ZOOM_PAN;
                cam.center = Vec2::new(center.x + lerp(-600.0, 600.0, k), center.y + lerp(-200.0, 200.0, k));
                Phase::Pan
            }
            t if t < 13.0 => {
                let k = (t - 10.0) / 3.0;
                cam.zoom = elerp(ZOOM_PAN, z_fit, k);
                let from = center + Vec2::new(600.0, 200.0);
                cam.center = from.lerp(center, k);
                Phase::ZoomOut
            }
            t if t < 17.5 && self.edits.len() < EDITS => {
                cam.center = center;
                cam.zoom = ZOOM_EDIT;
                self.edit(app, canvas);
                Phase::Edits
            }
            t if t < 20.0 => {
                cam.center = center;
                cam.zoom = z_fit;
                Phase::IdleEnd
            }
            _ => return None,
        };
        Some(phase)
    }

    fn edit(&mut self, app: &mut BitwerkApp, canvas: Vec2) {
        if self.targets.is_empty() {
            let (lo, hi) = app.camera.visible_rect(canvas);
            let mut ids = Vec::new();
            app.scene_query(lo, hi, |id| ids.push(id));
            ids.sort_unstable();
            ids.dedup();
            // Only components that have an output (so they can feed a wire).
            ids.retain(|id| app.circuit.component(*id).is_some_and(|c| c.kind.output_count() > 0));
            ids.truncate(64);
            if ids.is_empty() {
                return;
            }
            self.targets = ids;
        }
        let i = self.edits.len();
        let n = self.targets.len();
        let j = i / 8;
        let target = self.targets[(j * 7) % n];
        // The NOT added by step 2 of the current cycle, and its input pin.
        let not_in = self.added.last().map(|&id| PinRef { component: id, pin: 0 });
        let branch_seg = |app: &BitwerkApp| {
            let node = app.circuit.pin_node(not_in?)?;
            app.circuit.node(node)?.segments().first().copied()
        };
        let ok = match i % 8 {
            0 => app.circuit.component(target).map(|c| c.pos.offset(0, 1)).is_some_and(|to| app.edit_move(target, to)),
            1 => app.edit_rotate(target),
            2 => {
                let Some(c) = app.circuit.component(target).cloned() else { return };
                // With macros (`--stress-macros`): place another instance of
                // the target's macro instead of a NOT.
                let kind = if matches!(c.kind, ComponentKind::Macro(_)) { c.kind } else { ComponentKind::Not };
                match app.edit_place(kind, GridPos::new(c.pos.x + 1, c.pos.y + 6)) {
                    Ok(not) => {
                        self.added.push(not);
                        true
                    }
                    Err(_) => false,
                }
            }
            3 => {
                // Branch off the middle of a segment of the target's output
                // net (splits it: a junction) into the NOT's input.
                let out = PinRef {
                    component: target,
                    pin: app.circuit.component(target).map_or(0, |c| c.kind.input_count()),
                };
                let from = split_point(app, out).unwrap_or(WireEnd::Pin(out));
                not_in.is_some_and(|to| {
                    let via = vec![];
                    app.edit_wire(from, WireEnd::Pin(to), via).is_ok()
                })
            }
            4 => branch_seg(app).is_some_and(|s| app.apply_edit(Edit::DragSegment(s, 1)).is_ok()),
            5 if matches!(app.circuit.component(target).map(|c| c.kind), Some(ComponentKind::Macro(_))) => {
                // Redefine the target's macro (every instance follows; the
                // simulator recompiles in the background).
                let Some(ComponentKind::Macro(shape)) = app.circuit.component(target).map(|c| c.kind) else { return };
                let Some(mut def) = app.circuit.macros().def(shape.def).map(|d| (**d).clone()) else { return };
                def.datasheet = format!("revision {i}");
                app.redefine(shape.def, def).is_ok()
            }
            5 => {
                // Move a bend of the branch (geometry only).
                let bend = branch_seg(app).and_then(|s| {
                    let sg = app.circuit.segment(s)?;
                    [sg.a, sg.b].into_iter().find(|&n| app.circuit.node(n).is_some_and(bw_core::WireNode::is_free))
                });
                bend.is_some_and(|b| {
                    let to = app.circuit.node(b).map_or(GridPos::default(), |w| w.pos.offset(1, 0));
                    app.apply_edit(Edit::MoveNode(b, to)).is_ok()
                })
            }
            6 => branch_seg(app).is_some_and(|s| app.apply_edit(Edit::RemoveSegment(s)).is_ok()),
            _ => self.added.pop().is_some_and(|id| app.edit_delete(id)),
        };
        if !ok {
            tracing::warn!(edit = i, "scripted edit failed");
        }
        self.edits.push(EditRec { revision: app.revision, at: Instant::now(), frame: self.frame_idx, visible: None });
    }

    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    fn report(&self) {
        let mut out = String::new();
        let total = self.started.elapsed();
        let _ = writeln!(out, "\n=== bitwerk sweep summary ({:.1} s total) ===", total.as_secs_f64());
        // Missed vsyncs: wall time of a phase in refresh periods minus the
        // frames presented. Unlike single intervals this ignores pacing
        // jitter (a late frame followed by an early one costs nothing).
        let (period, valid_rate) = refresh_period(&self.frames);
        let not_presented = self.frames.iter().skip(1).filter(|r| !r.presented).count();
        if not_presented > 0 || !valid_rate {
            let _ = writeln!(
                out,
                "*** INVALID RUN: {not_presented} passes were not painted/presented (window occluded or minimized){} — \
                 numbers below cover presented frames only and must not be used ***",
                if valid_rate { "" } else { "; no plausible display refresh rate detected" }
            );
        }
        let _ = writeln!(
            out,
            "refresh period {period:.3} ms ({:.0} Hz); times in ms; 'missed' = estimated missed vsyncs",
            1000.0 / period
        );
        let _ = writeln!(
            out,
            "{:<10} {:>6} | {:>7} {:>7} {:>7} {:>7} {:>7} {:>6} {:>6} {:>6} | {:>7} {:>7} {:>7} | {:>7} {:>7}",
            "phase",
            "frames",
            "avg",
            "p50",
            "p95",
            "p99",
            "max",
            ">16.7",
            ">20",
            "missed",
            "cpu avg",
            "cpu p99",
            "cpu max",
            "prep99",
            "prepmx"
        );
        let mut row = |name: &str, recs: &[&FrameRec]| {
            if recs.is_empty() {
                return;
            }
            let dts: Vec<f64> = recs.iter().map(|r| r.dt).collect();
            let cpus: Vec<f64> = recs.iter().map(|r| r.cpu).collect();
            let preps: Vec<f64> = recs.iter().map(|r| r.prepare).collect();
            let s = Stats::of(&dts);
            let c = Stats::of(&cpus);
            let p = Stats::of(&preps);
            let missed = ((dts.iter().sum::<f64>() / period).round() - dts.len() as f64).max(0.0);
            let _ = writeln!(
                out,
                "{:<10} {:>6} | {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>6} {:>6} {:>6} | {:>7.2} {:>7.2} {:>7.2} | {:>7.2} {:>7.2}",
                name,
                recs.len(),
                s.avg,
                s.p50,
                s.p95,
                s.p99,
                s.max,
                dts.iter().filter(|&&d| d > BUDGET_MS).count(),
                dts.iter().filter(|&&d| d > 20.0).count(),
                missed,
                c.avg,
                c.p99,
                c.max,
                p.p99,
                p.max,
            );
        };
        let load: Vec<&FrameRec> =
            self.frames.iter().skip(1).filter(|r| r.phase == Phase::Load && r.presented).collect();
        row("load", &load);
        for ph in
            [Phase::IdleFit, Phase::ZoomIn, Phase::ZoomMid, Phase::Pan, Phase::ZoomOut, Phase::Edits, Phase::IdleEnd]
        {
            let recs: Vec<&FrameRec> = self.frames.iter().filter(|r| r.phase == ph && r.presented).collect();
            row(ph.name(), &recs);
        }
        // Whole sweep, excluding the load phase and the very first frame.
        let all: Vec<&FrameRec> =
            self.frames.iter().filter(|r| r.phase != Phase::Load && r.presented).skip(1).collect();
        row("SWEEP", &all);

        let lat: Vec<f64> = self.edits.iter().filter_map(|e| e.visible.map(|v| v.0)).collect();
        let lat_frames: Vec<f64> = self.edits.iter().filter_map(|e| e.visible.map(|v| v.1 as f64)).collect();
        let never = self.edits.iter().filter(|e| e.visible.is_none()).count();
        if !lat.is_empty() {
            let s = Stats::of(&lat);
            let f = Stats::of(&lat_frames);
            let _ = writeln!(
                out,
                "edit-to-visible ({} edits, {never} never shown): ms avg {:.2} p50 {:.2} p95 {:.2} max {:.2} | frames avg {:.2} p50 {:.0} max {:.0}",
                self.edits.len(),
                s.avg,
                s.p50,
                s.p95,
                s.max,
                f.avg,
                f.p50,
                f.max
            );
        }
        let drawn_max = self.frames.iter().map(|r| r.drawn).max().unwrap_or(0);
        let _ = writeln!(out, "max instances drawn in a frame: {drawn_max}");
        println!("{out}");
        tracing::info!("{out}");

        if let Some(path) = &self.csv {
            let mut csv =
                String::from("frame,phase,dt_ms,cpu_prev_ms,prepare_prev_ms,drawn_prev,presented_prev,events\n");
            for (i, r) in self.frames.iter().enumerate() {
                let _ = writeln!(
                    csv,
                    "{i},{},{:.3},{:.3},{:.3},{},{},{}",
                    r.phase.name(),
                    r.dt,
                    r.cpu,
                    r.prepare,
                    r.drawn,
                    u8::from(r.presented),
                    r.events
                );
            }
            if let Err(e) = std::fs::write(path, csv) {
                tracing::warn!("could not write {}: {e}", path.display());
            }
        }
    }
}

/// Common display refresh rates (Hz).
const REFRESH_RATES: [f64; 10] = [24.0, 30.0, 48.0, 50.0, 60.0, 75.0, 90.0, 100.0, 120.0, 144.0];
const MORE_RATES: [f64; 3] = [165.0, 200.0, 240.0];

/// Refresh period from the median interval of presented frames, snapped to
/// the nearest common rate. `false` if the median is not within 10 % of any
/// common rate (e.g. frames were not vsync-paced).
fn refresh_period(frames: &[FrameRec]) -> (f64, bool) {
    let mut d: Vec<f64> =
        frames.iter().skip(1).filter(|r| r.presented && r.phase != Phase::Load).map(|r| r.dt).collect();
    if d.len() < 30 {
        return (BUDGET_MS, false);
    }
    d.sort_by(f64::total_cmp);
    let median = d[d.len() / 2];
    let hz = 1000.0 / median.max(1e-3);
    let best = REFRESH_RATES
        .iter()
        .chain(&MORE_RATES)
        .copied()
        .min_by(|a, b| (a - hz).abs().total_cmp(&(b - hz).abs()))
        .unwrap_or(60.0);
    (1000.0 / best, (hz - best).abs() <= best * 0.1)
}

fn fit(app: &BitwerkApp, canvas: Vec2) -> (Vec2, f32) {
    let (lo, hi) = app.scene_bounds();
    let ext = (hi - lo).max(Vec2::ONE);
    let c = canvas.max(Vec2::splat(100.0));
    ((lo + hi) * 0.5, (c.x / ext.x).min(c.y / ext.y) * 0.95)
}

struct Stats {
    avg: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

impl Stats {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn of(v: &[f64]) -> Self {
        if v.is_empty() {
            return Self { avg: 0.0, p50: 0.0, p95: 0.0, p99: 0.0, max: 0.0 };
        }
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        let pct = |p: f64| s[((s.len() - 1) as f64 * p).round() as usize];
        Self {
            avg: s.iter().sum::<f64>() / s.len() as f64,
            p50: pct(0.5),
            p95: pct(0.95),
            p99: pct(0.99),
            max: pct(1.0),
        }
    }
}

/// Visits component ids of bodies in a world rectangle (helper for scripts).
pub fn bodies_in(scene: &bw_render::Scene, lo: Vec2, hi: Vec2, mut f: impl FnMut(ComponentId)) {
    scene.query(lo, hi, |it| {
        if let Item::Body(i) = it {
            if let Some(id) = scene.body_id(i) {
                f(id);
            }
        }
    });
}

/// A point in the middle of a segment of the net of `out` (a wire end that
/// splits it), if it has one long enough.
fn split_point(app: &BitwerkApp, out: PinRef) -> Option<WireEnd> {
    let start = app.circuit.pin_node(out)?;
    let mut found: Option<(SegmentId, GridPos)> = None;
    app.circuit.visit_net(
        start,
        |_, _| {},
        |s| {
            if found.is_some() {
                return;
            }
            if let Some((a, b)) = app.circuit.segment_ends(s) {
                if (a.x - b.x).abs() + (a.y - b.y).abs() >= 2 {
                    found = Some((s, GridPos::new((a.x + b.x) / 2, (a.y + b.y) / 2)));
                }
            }
        },
    );
    found.map(|(s, p)| WireEnd::OnSegment(s, p))
}
