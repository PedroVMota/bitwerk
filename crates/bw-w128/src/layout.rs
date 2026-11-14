//! Column placement and channel routing of a logical netlist into a bitwerk
//! circuit (a macro body or the root board).
//!
//! The generator describes each circuit as components assigned to
//! **columns** plus nets (a driver pin and reader pins). [`Body::build_into`]
//! then:
//!
//! 1. **Places** each column top to bottom (insertion order, with a gap),
//!    optionally aligning a component so that one of its pins lines up with
//!    a pin in an earlier column (straight wires for datapaths). A component
//!    without an exact alignment may move down one unit when that avoids
//!    sharing a row with an unrelated pin across the channel (see 3).
//! 2. Gives every net one vertical **track** in each channel (the space
//!    between two columns) where it has pins. Pins reach their track with a
//!    horizontal stub. A net with pins in several channels joins its tracks
//!    with a horizontal **feedthrough row** above or below the whole layout
//!    (rows are shared by nets whose spans do not overlap, left-edge
//!    algorithm). Those rows are the visible "buses".
//! 3. Orders the tracks of a channel so that no two nets ever share a
//!    segment: when the pin on the left edge of a channel and the pin on the
//!    right edge sit on the same row and belong to different nets, the left
//!    pin's net must get the track further left (its stub ends before the
//!    other one starts). The ordering is a topological sort; a cycle (which
//!    placement step 1 tries to avoid) is reported in [`Layout::conflicts`].
//! 4. Emits the wires as bitwerk edits: each net is a tree (stubs, tracks,
//!    feedthrough row) added pin by pin from its driver; branches join the
//!    existing tree in the middle of a segment (a junction).
//!
//! Only left and right pins are supported (no tri-state buffers or
//! flip-flops with preset/clear in generated bodies), and every component
//! is placed unrotated.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use bw_core::{
    Circuit, Color, ComponentId, ComponentKind, Edit, GateOp, GridPos, MacroPin, Note, PinRef, PinSide, Rotation,
    SegmentId, TagDir, WireEnd,
};

/// Grid rows taken by `lines` lines of text of `height` tenths of a unit.
fn text_rows(height: u8, lines: i32) -> i32 {
    (i32::from(height) * 16 + 99) / 100 * lines + 1
}

/// Row → (net of the pin on the channel's left edge, net of the pin on its
/// right edge).
type RowNets = HashMap<i32, (Option<usize>, Option<usize>)>;

/// Where a net crossing columns runs horizontally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feed {
    /// Through a free row of every column it crosses.
    Row(i32),
    /// Above or below the whole layout.
    Top,
    Bottom,
}

/// Handle of a component inside a [`Body`].
pub type H = usize;

/// An output pin (a net driver) of a component in a [`Body`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sig {
    pub comp: H,
    pub pin: u8,
}

/// Column of the output tags (resolved to the last column + 1).
pub const LAST: usize = usize::MAX;

/// Free space (grid units) between a column edge and its first track.
const MARGIN: i32 = 2;
/// Default vertical gap between two components of a column.
const GAP: i32 = 2;

#[derive(Debug, Clone)]
struct Spec {
    kind: ComponentKind,
    col: usize,
    /// Extra space above (for a label).
    gap: i32,
    /// Align pin `.0` of this component with the pin `.1`.
    align: Option<(u8, (H, u8))>,
    /// Exact y (overrides stacking; must not overlap).
    at_y: Option<i32>,
    color: Option<Color>,
    label: Option<(String, u8, Color)>,
}

/// A logical circuit waiting for placement and routing.
#[derive(Debug, Clone, Default)]
pub struct Body {
    specs: Vec<Spec>,
    /// Union-find over pins: pin → parent pin.
    parent: HashMap<(H, u8), (H, u8)>,
    tags: Vec<(H, String, TagDir)>,
    title: Option<String>,
    /// Gap after the components of a column, by column.
    col_gap: HashMap<usize, i32>,
}

/// Result of [`Body::build_into`].
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// Component id of each handle.
    pub ids: Vec<ComponentId>,
    /// Interface pins (tags) in order, ready for a [`bw_core::MacroDef`].
    pub pins: Vec<MacroPin>,
    /// Track-order constraints that had to be broken (shared rows).
    pub conflicts: usize,
    /// Bounding box of the layout (min, max).
    pub min: GridPos,
    pub max: GridPos,
}

impl Body {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a component to column `col`.
    pub fn add(&mut self, kind: ComponentKind, col: usize) -> H {
        self.specs.push(Spec { kind, col, gap: GAP, align: None, at_y: None, color: None, label: None });
        self.specs.len() - 1
    }

    #[must_use]
    pub fn kind(&self, h: H) -> ComponentKind {
        self.specs[h].kind
    }

    /// Output `k` of component `h`.
    #[must_use]
    pub fn out(&self, h: H, k: u8) -> Sig {
        Sig { comp: h, pin: self.specs[h].kind.input_count() + k }
    }

    /// Space above component `h` (default [`GAP`]).
    pub fn gap(&mut self, h: H, gap: i32) {
        self.specs[h].gap = gap;
    }

    /// Exact vertical position of `h`.
    pub fn at_y(&mut self, h: H, y: i32) {
        self.specs[h].at_y = Some(y);
    }

    /// Aligns input/pin `pin` of `h` with signal `s` (if `s` is placed in
    /// an earlier column).
    pub fn align(&mut self, h: H, pin: u8, s: Sig) {
        self.specs[h].align = Some((pin, (s.comp, s.pin)));
    }

    /// Aligns pin `pin` of `h` with pin `other_pin` of `other`.
    pub fn align_pins(&mut self, h: H, pin: u8, other: H, other_pin: u8) {
        self.specs[h].align = Some((pin, (other, other_pin)));
    }

    pub fn color(&mut self, h: H, c: Color) {
        self.specs[h].color = Some(c);
    }

    /// A text label just above component `h` (height in tenths of a unit).
    pub fn label(&mut self, h: H, text: &str, height: u8, color: Color) {
        let lines = i32::try_from(text.lines().count()).unwrap_or(1);
        let need = text_rows(height, lines);
        self.specs[h].gap = self.specs[h].gap.max(need + 1);
        self.specs[h].label = Some((text.to_string(), height, color));
    }

    /// A title above the whole layout.
    pub fn title(&mut self, text: &str) {
        self.title = Some(text.to_string());
    }

    /// Extra space between components of column `col`.
    pub fn column_gap(&mut self, col: usize, gap: i32) {
        self.col_gap.insert(col, gap);
    }

    fn find(&mut self, p: (H, u8)) -> (H, u8) {
        let mut r = p;
        while let Some(&q) = self.parent.get(&r) {
            if q == r {
                break;
            }
            r = q;
        }
        // Path compression.
        let mut x = p;
        while let Some(&q) = self.parent.get(&x) {
            if q == x {
                break;
            }
            self.parent.insert(x, r);
            x = q;
        }
        r
    }

    /// Connects pin `pin` of `h` to the net of `s`.
    pub fn connect(&mut self, s: Sig, h: H, pin: u8) {
        let a = self.find((s.comp, s.pin));
        let b = self.find((h, pin));
        self.parent.entry(a).or_insert(a);
        if a != b {
            self.parent.insert(b, a);
        }
    }

    /// Joins two nets (e.g. tri-state outputs on one bus).
    pub fn join(&mut self, a: Sig, b: Sig) {
        self.connect(a, b.comp, b.pin);
    }

    /// An input tag (macro interface pin) in column 0.
    pub fn input(&mut self, name: &str) -> Sig {
        let h = self.add(ComponentKind::Tag { dir: TagDir::In, tristate: false }, 0);
        self.tags.push((h, name.to_string(), TagDir::In));
        self.out(h, 0)
    }

    /// An output tag fed by `s`, in the last column, aligned with `s`.
    pub fn output(&mut self, name: &str, s: Sig) -> H {
        let h = self.add(ComponentKind::Tag { dir: TagDir::Out, tristate: false }, LAST);
        self.tags.push((h, name.to_string(), TagDir::Out));
        self.connect(s, h, 0);
        self.align(h, 0, s);
        h
    }

    /// A gate in column `col` fed by `ins`.
    pub fn gate(&mut self, op: GateOp, ins: &[Sig], col: usize) -> Sig {
        let n = u8::try_from(ins.len()).unwrap_or(u8::MAX);
        let h = self.add(ComponentKind::Gate { op, inputs: n }, col);
        for (k, s) in ins.iter().enumerate() {
            self.connect(*s, h, u8::try_from(k).unwrap_or(u8::MAX));
        }
        self.out(h, 0)
    }

    pub fn not(&mut self, s: Sig, col: usize) -> Sig {
        let h = self.add(ComponentKind::Not, col);
        self.connect(s, h, 0);
        self.out(h, 0)
    }

    /// Column of the component driving `s`.
    #[must_use]
    pub fn col_of(&self, s: Sig) -> usize {
        self.specs[s.comp].col
    }

    /// A gate one column right of its rightmost input (levelized logic).
    pub fn g(&mut self, op: GateOp, ins: &[Sig]) -> Sig {
        let col = ins.iter().map(|&s| self.col_of(s)).max().unwrap_or(0) + 1;
        self.gate(op, ins, col)
    }

    /// An inverter one column right of its input.
    pub fn n(&mut self, s: Sig) -> Sig {
        let col = self.col_of(s) + 1;
        self.not(s, col)
    }

    /// Feeds inputs `first..` of `h` from `sigs`.
    pub fn feed(&mut self, h: H, first: u8, sigs: &[Sig]) {
        for (k, s) in sigs.iter().enumerate() {
            self.connect(*s, h, first + u8::try_from(k).unwrap_or(u8::MAX));
        }
    }

    /// Outputs `first..first + n` of `h`.
    #[must_use]
    pub fn outs(&self, h: H, first: u8, n: u8) -> Vec<Sig> {
        (first..first + n).map(|k| self.out(h, k)).collect()
    }

    /// Lays the body out and adds it to `c` (an empty body or root circuit).
    ///
    /// # Panics
    /// On pins the router does not support (top / bottom) or if the model
    /// rejects a generated edit (a generator bug).
    #[must_use]
    #[allow(clippy::too_many_lines, reason = "one pass per layout phase, kept together")]
    pub fn build_into(mut self, c: &mut Circuit) -> Layout {
        let ncols = self.specs.iter().filter(|s| s.col != LAST).map(|s| s.col + 1).max().unwrap_or(0);
        for s in &mut self.specs {
            if s.col == LAST {
                s.col = ncols;
            }
        }
        let ncols = ncols + 1;
        // Nets: root pin → pins.
        let mut net_of: HashMap<(H, u8), usize> = HashMap::new();
        let mut nets: Vec<Vec<(H, u8)>> = Vec::new();
        let keys: Vec<(H, u8)> = self.parent.keys().copied().collect();
        let mut sorted = keys;
        sorted.sort_unstable();
        for p in sorted {
            let r = self.find(p);
            let n = *net_of.entry(r).or_insert_with(|| {
                nets.push(Vec::new());
                nets.len() - 1
            });
            net_of.insert(p, n);
        }
        for (p, &n) in &net_of {
            nets[n].push(*p);
        }
        for n in &mut nets {
            n.sort_unstable();
            n.dedup();
        }
        let net_at = |p: (H, u8)| net_of.get(&p).copied();

        // ---- placement: y ----
        let mut y_of = vec![0i32; self.specs.len()];
        let mut placed = vec![false; self.specs.len()];
        let pin_off = |kind: &ComponentKind, pin: u8| kind.pin_offset(pin).unwrap_or((0, 0));
        let side = |kind: &ComponentKind, pin: u8| kind.pin_side(pin).unwrap_or(PinSide::Left);
        let mut by_col: Vec<Vec<H>> = vec![Vec::new(); ncols];
        for (h, s) in self.specs.iter().enumerate() {
            by_col[s.col].push(h);
        }
        // Rows of right-edge pins of the previous column → net.
        let mut prev_rows: HashMap<i32, usize> = HashMap::new();
        for (col, members) in by_col.iter().enumerate() {
            let gap_col = self.col_gap.get(&col).copied().unwrap_or(0);
            let mut next_free = i32::MIN / 4;
            for &h in members {
                let s = &self.specs[h];
                let (_, height) = s.kind.size();
                let min_y = if next_free == i32::MIN / 4 { 0 } else { next_free + s.gap + gap_col };
                let mut exact = false;
                let mut y = min_y;
                if let Some(at) = s.at_y {
                    y = at;
                    exact = true;
                } else if let Some((my_pin, (oh, op))) = s.align {
                    if placed[oh] {
                        let oy = y_of[oh] + pin_off(&self.specs[oh].kind, op).1;
                        let want = oy - pin_off(&s.kind, my_pin).1;
                        if want >= min_y {
                            y = want;
                            exact = true;
                        }
                    }
                }
                if !exact {
                    // Avoid rows of unrelated pins across the channel.
                    let conflicts = |y: i32| -> usize {
                        (0..s.kind.pin_count())
                            .filter(|&p| side(&s.kind, p) == PinSide::Left)
                            .filter(|&p| {
                                let row = y + pin_off(&s.kind, p).1;
                                prev_rows.get(&row).is_some_and(|&n| Some(n) != net_at((h, p)))
                            })
                            .count()
                    };
                    if conflicts(y + 1) < conflicts(y) {
                        y += 1;
                    }
                }
                y_of[h] = y;
                placed[h] = true;
                next_free = next_free.max(y + height);
            }
            prev_rows.clear();
            for &h in members {
                let k = &self.specs[h].kind;
                for p in 0..k.pin_count() {
                    if side(k, p) == PinSide::Right {
                        if let Some(n) = net_at((h, p)) {
                            prev_rows.insert(y_of[h] + pin_off(k, p).1, n);
                        }
                    }
                }
            }
        }

        // ---- channels and tracks ----
        // Channel index ch = col + 1 for right pins of `col`, col for left
        // pins of `col` (channel 0 is left of column 0).
        let nch = ncols + 1;
        let mut ch_nets: Vec<BTreeMap<usize, Vec<i32>>> = vec![BTreeMap::new(); nch];
        // Row → (left-edge net, right-edge net) per channel.
        let mut rows: Vec<RowNets> = vec![HashMap::new(); nch];
        for (n, pins) in nets.iter().enumerate() {
            for &(h, p) in pins {
                let s = &self.specs[h];
                let (_, oy) = pin_off(&s.kind, p);
                let y = y_of[h] + oy;
                let (ch, left_edge) = match side(&s.kind, p) {
                    PinSide::Right => (s.col + 1, true),
                    PinSide::Left => (s.col, false),
                    other => panic!("{:?} pin {p} on side {other:?} is not supported by the router", s.kind),
                };
                ch_nets[ch].entry(n).or_default().push(y);
                let e = rows[ch].entry(y).or_default();
                if left_edge {
                    e.0 = Some(n);
                } else {
                    e.1 = Some(n);
                }
            }
        }
        // ---- which nets leave their channel, and on which side ----
        let mut top = i32::MAX;
        let mut bottom = i32::MIN;
        for (h, s) in self.specs.iter().enumerate() {
            let (_, hh) = s.kind.size();
            top = top.min(y_of[h] - s.gap);
            bottom = bottom.max(y_of[h] + hh);
        }
        if self.specs.is_empty() {
            top = 0;
            bottom = 0;
        }
        // ---- crossing columns: through a free gap, else around ----
        // Occupied rows of each column: bodies, their labels, one row of
        // margin.
        let mut occ: Vec<Vec<(i32, i32)>> = vec![Vec::new(); ncols];
        for (h, s) in self.specs.iter().enumerate() {
            let (_, hh) = s.kind.size();
            let label =
                s.label.as_ref().map_or(0, |(t, ht, _)| text_rows(*ht, i32::try_from(t.lines().count()).unwrap_or(1)));
            occ[s.col].push((y_of[h] - label - 1, y_of[h] + hh + 1));
        }
        let mut feed: HashMap<usize, Feed> = HashMap::new();
        let mut multi: Vec<(usize, usize, usize, i32, i32)> = Vec::new();
        for n in 0..nets.len() {
            let chans: Vec<usize> = (0..nch).filter(|&ch| ch_nets[ch].contains_key(&n)).collect();
            if chans.len() < 2 {
                continue;
            }
            let ys = chans.iter().flat_map(|ch| ch_nets[*ch][&n].iter().copied());
            let (pmin, pmax) = ys.fold((i32::MAX, i32::MIN), |(a, b), y| (a.min(y), b.max(y)));
            multi.push((n, chans[0], chans[chans.len() - 1], pmin, pmax));
        }
        // Short hops first, then top to bottom.
        multi.sort_unstable_by_key(|&(n, lo, hi, pmin, _)| (hi - lo, pmin, n));
        let mut used: HashMap<i32, Vec<(usize, usize)>> = HashMap::new();
        for &(n, lo, hi, pmin, pmax) in &multi {
            let free = |y: i32| -> bool {
                (lo..hi).all(|col| !occ[col].iter().any(|&(a, b)| a <= y && y <= b))
                    && rows[lo].get(&y).is_none_or(|e| e.0.is_none_or(|m| m == n))
                    && rows[hi].get(&y).is_none_or(|e| e.1.is_none_or(|m| m == n))
                    && !used.get(&y).is_some_and(|v| v.iter().any(|&(a, b)| a <= hi && lo <= b))
            };
            let center = (pmin + pmax) / 2;
            let mut inside: Vec<i32> = (pmin..=pmax).collect();
            inside.sort_by_key(|&y| ((y - center).abs(), y));
            let outside = (1..).map_while(|d: i32| {
                let (a, b) = (pmin - d, pmax + d);
                (a > top - 1 || b < bottom + 1).then_some([a, b])
            });
            let pick = inside.into_iter().chain(outside.flatten()).find(|&y| y > top - 1 && y < bottom + 1 && free(y));
            if let Some(y) = pick {
                used.entry(y).or_default().push((lo, hi));
                feed.insert(n, Feed::Row(y));
            } else {
                // Around everything, on the nearer side.
                let (mut up, mut down) = (0i64, 0i64);
                for ch in lo..=hi {
                    if let Some(ys) = ch_nets[ch].get(&n) {
                        up += i64::from(ys.iter().min().copied().unwrap_or(0) - top);
                        down += i64::from(bottom - ys.iter().max().copied().unwrap_or(0));
                    }
                }
                feed.insert(n, if up <= down { Feed::Top } else { Feed::Bottom });
            }
        }

        // ---- track order per channel ----
        let mut conflicts = 0;
        let mut order: Vec<Vec<usize>> = Vec::with_capacity(nch);
        for ch in 0..nch {
            let (ord, broken) = order_tracks(&ch_nets[ch], &rows[ch], &feed);
            conflicts += broken;
            order.push(ord);
        }

        // ---- x positions ----
        let width_of: Vec<i32> =
            (0..ncols).map(|col| by_col[col].iter().map(|&h| self.specs[h].kind.size().0).max().unwrap_or(0)).collect();
        let ch_width = |ch: usize| -> i32 {
            let t = i32::try_from(order[ch].len()).unwrap_or(0);
            if t == 0 { 2 * MARGIN } else { t + 2 * MARGIN }
        };
        let mut col_x = vec![0i32; ncols];
        let mut x = ch_width(0);
        for col in 0..ncols {
            col_x[col] = x;
            x += width_of[col] + ch_width(col + 1);
        }
        // Track x per (channel, net).
        let mut track_x: HashMap<(usize, usize), i32> = HashMap::new();
        for (ch, ord) in order.iter().enumerate() {
            let left = if ch == 0 { 0 } else { col_x[ch - 1] + width_of[ch - 1] };
            for (k, &n) in ord.iter().enumerate() {
                track_x.insert((ch, n), left + MARGIN + i32::try_from(k).unwrap_or(0));
            }
        }

        // ---- rows around the layout ----
        // Nested like a bus: a row spanning another one lies further out, so
        // parallel nets do not cross; nets whose spans are disjoint share a
        // row.
        let mut feed_y: HashMap<usize, i32> =
            feed.iter().filter_map(|(&n, f)| if let Feed::Row(y) = f { Some((n, *y)) } else { None }).collect();
        for is_top in [true, false] {
            let side_of = if is_top { Feed::Top } else { Feed::Bottom };
            let mut list: Vec<(i32, i32, i32, usize)> = feed
                .iter()
                .filter(|&(_, f)| *f == side_of)
                .map(|(&n, _)| {
                    let xs: Vec<i32> =
                        (0..nch).filter(|&ch| ch_nets[ch].contains_key(&n)).map(|ch| track_x[&(ch, n)]).collect();
                    let lo = xs.iter().min().copied().unwrap_or(0);
                    let hi = xs.iter().max().copied().unwrap_or(0);
                    (hi - lo, lo, hi, n)
                })
                .collect();
            list.sort_unstable();
            let mut placed: Vec<(i32, i32, i32)> = Vec::with_capacity(list.len());
            for (_, lo, hi, n) in list {
                let r = placed.iter().filter(|p| p.0 <= hi && lo <= p.1).map(|p| p.2 + 1).max().unwrap_or(0);
                placed.push((lo, hi, r));
                feed_y.insert(n, if is_top { top - 2 - r } else { bottom + 2 + r });
            }
        }

        // ---- components ----
        let mut ids = Vec::with_capacity(self.specs.len());
        for (h, s) in self.specs.iter().enumerate() {
            let id = c
                .add_component(s.kind, GridPos::new(col_x[s.col], y_of[h]), Rotation::R0)
                .unwrap_or_else(|e| panic!("cannot place {:?}: {e}", s.kind));
            if let Some(col) = s.color {
                c.set_color(id, Some(col)).unwrap_or_else(|e| panic!("colour: {e}"));
            }
            ids.push(id);
        }
        // Labels above components.
        for (h, s) in self.specs.iter().enumerate() {
            if let Some((text, height, color)) = &s.label {
                let lines = i32::try_from(text.lines().count()).unwrap_or(1);
                let dy = text_rows(*height, lines);
                add_note(c, GridPos::new(col_x[s.col], y_of[h] - dy), text, *height, true, Some(*color));
            }
        }

        // ---- wires ----
        for (n, pins) in nets.iter().enumerate() {
            if pins.len() < 2 {
                continue;
            }
            let pos = |&(h, p): &(H, u8)| -> (i32, i32) {
                let (ox, oy) = pin_off(&self.specs[h].kind, p);
                (col_x[self.specs[h].col] + ox, y_of[h] + oy)
            };
            // Tree geometry.
            let mut adj: BTreeMap<(i32, i32), BTreeSet<(i32, i32)>> = BTreeMap::new();
            let mut link = |a: (i32, i32), b: (i32, i32)| {
                if a != b {
                    adj.entry(a).or_default().insert(b);
                    adj.entry(b).or_default().insert(a);
                }
            };
            let fy = feed_y.get(&n).copied();
            let mut row_pts: Vec<i32> = Vec::new();
            for ch in 0..nch {
                let Some(ys) = ch_nets[ch].get(&n) else { continue };
                let tx = track_x[&(ch, n)];
                let mut stops: Vec<i32> = ys.clone();
                if let Some(fy) = fy {
                    stops.push(fy);
                    row_pts.push(tx);
                }
                stops.sort_unstable();
                stops.dedup();
                for w in stops.windows(2) {
                    link((tx, w[0]), (tx, w[1]));
                }
                for pin in pins {
                    let s = &self.specs[pin.0];
                    let pch = match side(&s.kind, pin.1) {
                        PinSide::Right => s.col + 1,
                        _ => s.col,
                    };
                    if pch == ch {
                        let p = pos(pin);
                        link(p, (tx, p.1));
                    }
                }
            }
            if let Some(fy) = fy {
                row_pts.sort_unstable();
                for w in row_pts.windows(2) {
                    link((w[0], fy), (w[1], fy));
                }
            }
            // Root: the driver (an output pin), else the first pin.
            let root = pins.iter().copied().find(|&(h, p)| p >= self.specs[h].kind.input_count()).unwrap_or(pins[0]);
            let root_pos = pos(&root);
            let mut parent: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
            let mut seen: HashSet<(i32, i32)> = HashSet::from([root_pos]);
            let mut q = VecDeque::from([root_pos]);
            let mut bfs: Vec<(i32, i32)> = Vec::new();
            while let Some(v) = q.pop_front() {
                bfs.push(v);
                if let Some(ns) = adj.get(&v) {
                    for &w in ns {
                        if seen.insert(w) {
                            parent.insert(w, v);
                            q.push_back(w);
                        }
                    }
                }
            }
            let pin_at: HashMap<(i32, i32), PinRef> =
                pins.iter().map(|pr| (pos(pr), PinRef { component: ids[pr.0], pin: pr.1 })).collect();
            let mut built: HashSet<(i32, i32)> = HashSet::from([root_pos]);
            let mut segs: Vec<SegmentId> = Vec::new();
            for v in bfs {
                let Some(&pin) = pin_at.get(&v) else { continue };
                if v == root_pos {
                    continue;
                }
                let mut path = vec![v];
                let mut cur = v;
                while !built.contains(&cur) {
                    cur = *parent.get(&cur).unwrap_or_else(|| panic!("net {n}: pin not on the tree"));
                    path.push(cur);
                }
                let attach = cur;
                let mut via = Vec::new();
                for w in path.windows(3) {
                    let (a, b, d) = (w[0], w[1], w[2]);
                    if (a.0 == b.0) != (b.0 == d.0) {
                        via.push(GridPos::new(b.0, b.1));
                    }
                }
                for p in &path {
                    built.insert(*p);
                }
                let to = if let Some(&tp) = pin_at.get(&attach) {
                    WireEnd::Pin(tp)
                } else {
                    segs.retain(|&s| c.segment(s).is_some());
                    segs.sort_unstable();
                    segs.dedup();
                    let gp = GridPos::new(attach.0, attach.1);
                    let s = segs
                        .iter()
                        .copied()
                        .find(|&s| c.segment_ends(s).is_some_and(|(a, b)| on_seg(gp, a, b)))
                        .unwrap_or_else(|| panic!("net {n}: no segment at {attach:?}"));
                    WireEnd::OnSegment(s, gp)
                };
                let applied = c
                    .apply(&Edit::AddWire { from: WireEnd::Pin(pin), to, via })
                    .unwrap_or_else(|e| panic!("net {n}: wire refused: {e}"));
                segs.extend(applied.segments.iter().copied().filter(|&s| c.segment(s).is_some()));
            }
        }

        let mut lay = Layout { ids, pins: Vec::new(), conflicts, ..Layout::default() };
        let mut min = GridPos::new(0, top.min(feed_y.values().copied().min().unwrap_or(top)));
        let max = GridPos::new(x, bottom.max(feed_y.values().copied().max().unwrap_or(bottom)));
        if let Some(t) = &self.title {
            let y = min.y - 6;
            add_note(c, GridPos::new(0, y), t, 30, true, Some(Color::White));
            min.y = y;
        }
        lay.min = min;
        lay.max = max;
        // Interface pins: inputs and outputs in creation order.
        for dir in [TagDir::In, TagDir::Out] {
            for (h, name, d) in &self.tags {
                if *d == dir {
                    lay.pins.push(MacroPin { tag: lay.ids[*h], name: name.clone(), dil: 0 });
                }
            }
        }
        lay
    }
}

/// Rows of a net's pins in one channel: left-edge pins (right side of the
/// column to the left), right-edge pins, and the vertical span of its track.
struct TrackNet {
    left: Vec<i32>,
    right: Vec<i32>,
    lo: i32,
    hi: i32,
}

/// Crossings when net `i`'s track is left of net `j`'s: `i`'s stubs to the
/// right edge cross `j`'s track, `j`'s stubs from the left edge cross `i`'s.
fn crossings(i: &TrackNet, j: &TrackNet) -> usize {
    let inside = |rows: &[i32], lo: i32, hi: i32| {
        let a = rows.partition_point(|&y| y <= lo);
        let b = rows.partition_point(|&y| y < hi);
        b.saturating_sub(a)
    };
    inside(&i.right, j.lo, j.hi) + inside(&j.left, i.lo, i.hi)
}

/// Orders the tracks of one channel: a topological order of the shared-row
/// constraints (the net with the left-edge pin goes left), then adjacent
/// swaps while they remove crossings. Returns the order and the number of
/// constraints that had to be broken (cycles).
fn order_tracks(nets: &BTreeMap<usize, Vec<i32>>, rows: &RowNets, feed: &HashMap<usize, Feed>) -> (Vec<usize>, usize) {
    let members: Vec<usize> = nets.keys().copied().collect();
    let mut info: HashMap<usize, TrackNet> = HashMap::new();
    for &n in &members {
        let ys = &nets[&n];
        let (mut lo, mut hi) = (ys.iter().min().copied().unwrap_or(0), ys.iter().max().copied().unwrap_or(0));
        match feed.get(&n) {
            Some(Feed::Top) => lo = i32::MIN / 2,
            Some(Feed::Bottom) => hi = i32::MAX / 2,
            Some(Feed::Row(y)) => {
                lo = lo.min(*y);
                hi = hi.max(*y);
            }
            None => {}
        }
        info.insert(n, TrackNet { left: Vec::new(), right: Vec::new(), lo, hi });
    }
    let mut must: HashSet<(usize, usize)> = HashSet::new();
    let mut succ: HashMap<usize, BTreeSet<usize>> = HashMap::new();
    let mut indeg: HashMap<usize, usize> = members.iter().map(|&n| (n, 0)).collect();
    let mut row_list: Vec<(i32, Option<usize>, Option<usize>)> = rows.iter().map(|(&y, &(l, r))| (y, l, r)).collect();
    row_list.sort_unstable();
    for (y, l, r) in row_list {
        if let Some(l) = l {
            if let Some(t) = info.get_mut(&l) {
                t.left.push(y);
            }
        }
        if let Some(r) = r {
            if let Some(t) = info.get_mut(&r) {
                t.right.push(y);
            }
        }
        if let (Some(l), Some(r)) = (l, r) {
            if l != r && succ.entry(l).or_default().insert(r) {
                must.insert((l, r));
                *indeg.entry(r).or_default() += 1;
            }
        }
    }
    // Initial order: topological, topmost first.
    let key = |n: usize| nets[&n].iter().min().copied().unwrap_or(0);
    let mut ready: BTreeSet<(i32, usize)> = members.iter().filter(|n| indeg[n] == 0).map(|&n| (key(n), n)).collect();
    let mut out = Vec::with_capacity(members.len());
    let mut done: HashSet<usize> = HashSet::new();
    let mut broken = 0;
    while out.len() < members.len() {
        let next = if let Some(&first) = ready.iter().next() {
            ready.remove(&first);
            first.1
        } else {
            broken += 1;
            let n = members.iter().copied().filter(|n| !done.contains(n)).min_by_key(|&n| key(n)).unwrap_or(0);
            indeg.insert(n, 0);
            n
        };
        if !done.insert(next) {
            continue;
        }
        out.push(next);
        if let Some(ss) = succ.get(&next) {
            for &m in ss {
                if let Some(d) = indeg.get_mut(&m) {
                    if *d > 0 {
                        *d -= 1;
                        if *d == 0 && !done.contains(&m) {
                            ready.insert((key(m), m));
                        }
                    }
                }
            }
        }
    }
    // Local improvement: swapping two neighbours only changes their pair.
    if broken == 0 {
        for _pass in 0..out.len().min(64) {
            let mut improved = false;
            for k in 0..out.len().saturating_sub(1) {
                let (i, j) = (out[k], out[k + 1]);
                if must.contains(&(i, j)) {
                    continue;
                }
                let (ti, tj) = (&info[&i], &info[&j]);
                if crossings(tj, ti) < crossings(ti, tj) {
                    out.swap(k, k + 1);
                    improved = true;
                }
            }
            if !improved {
                break;
            }
        }
    }
    (out, broken)
}

fn on_seg(p: GridPos, a: GridPos, b: GridPos) -> bool {
    if a.x == b.x {
        p.x == a.x && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y)
    } else {
        p.y == a.y && p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x)
    }
}

/// Adds a text annotation at `at`.
///
/// # Panics
/// If the model rejects the note (a generator bug).
pub fn add_note(c: &mut Circuit, at: GridPos, text: &str, height: u8, bold: bool, color: Option<Color>) -> ComponentId {
    let id =
        c.add_component(ComponentKind::Text { w: 1, h: 1 }, at, Rotation::R0).unwrap_or_else(|e| panic!("note: {e}"));
    let note = Note { text: text.to_string(), height, bold, italic: false, overbar: false };
    c.set_note(id, note).unwrap_or_else(|e| panic!("note text {text:?}: {e}"));
    if color.is_some() {
        c.set_color(id, color).unwrap_or_else(|e| panic!("note colour: {e}"));
    }
    id
}

fn find(p: &mut [usize], mut x: usize) -> usize {
    while p[x] != x {
        p[x] = p[p[x]];
        x = p[x];
    }
    x
}

/// Collinear wire intervals: (horizontal?, row or column) → (from, to, net).
type Lines = HashMap<(bool, i32), Vec<(i32, i32, usize)>>;

/// Geometry problems of a laid-out circuit: segments of different nets
/// that overlap, and wire nodes lying on a segment of another net (both
/// look like connections that are not there).
#[must_use]
pub fn overlaps(c: &Circuit) -> Vec<String> {
    // Net of each node: union-find over segments.
    let bound = c.node_id_bound();
    let mut parent: Vec<usize> = (0..bound).collect();
    let segs: Vec<(SegmentId, usize, usize)> =
        c.segments().map(|(s, sg)| (s, sg.a.0 as usize, sg.b.0 as usize)).collect();
    for &(_, a, b) in &segs {
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    }
    let mut problems = Vec::new();
    // Lines: (horizontal?, coordinate) → intervals (lo, hi, net).
    let mut lines: Lines = HashMap::new();
    for &(s, a, _) in &segs {
        let Some((p, q)) = c.segment_ends(s) else { continue };
        let net = find(&mut parent, a);
        if p.y == q.y {
            lines.entry((true, p.y)).or_default().push((p.x.min(q.x), p.x.max(q.x), net));
        } else {
            lines.entry((false, p.x)).or_default().push((p.y.min(q.y), p.y.max(q.y), net));
        }
    }
    for (key, list) in &mut lines {
        list.sort_unstable();
        let mut reach: Vec<(i32, usize)> = Vec::new();
        for &(lo, hi, net) in list.iter() {
            for &(end, other) in &reach {
                if other != net && end >= lo {
                    problems.push(format!(
                        "overlap on {} {} at {lo}..{hi}",
                        if key.0 { "row" } else { "column" },
                        key.1
                    ));
                }
            }
            reach.retain(|&(end, _)| end >= lo);
            reach.push((hi, net));
        }
    }
    for (id, w) in c.nodes() {
        let net = find(&mut parent, id.0 as usize);
        for (key, along) in [((true, w.pos.y), w.pos.x), ((false, w.pos.x), w.pos.y)] {
            if let Some(list) = lines.get(&key) {
                if list.iter().any(|&(lo, hi, n)| n != net && lo <= along && along <= hi) {
                    problems.push(format!("node at ({}, {}) lies on another net", w.pos.x, w.pos.y));
                }
            }
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

/// The circuit's component boxes (`R x0 y0 x1 y1 label`) and wire segments
/// (`L x0 y0 x1 y1`), one per line: a layout debugging aid.
#[must_use]
pub fn geometry_text(c: &Circuit) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for comp in c.components() {
        let b = comp.bounds();
        let _ = writeln!(out, "R {} {} {} {} {}", b.min.x, b.min.y, b.max.x, b.max.y, comp.kind.label());
    }
    for (s, _) in c.segments() {
        if let Some((a, b)) = c.segment_ends(s) {
            let _ = writeln!(out, "L {} {} {} {}", a.x, a.y, b.x, b.y);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_exactly_the_requested_nets() {
        // A half adder whose inputs fan out, a feedback loop through a
        // flip-flop and an output read in a later column (feedthrough).
        let mut b = Body::new();
        let a = b.input("A");
        let bb = b.input("B");
        let x = b.gate(GateOp::Xor, &[a, bb], 1);
        let n = b.gate(GateOp::And, &[a, bb], 1);
        let ff = b.add(crate::blocks::DFF, 2);
        let q = b.out(ff, 0);
        let o = b.gate(GateOp::Or, &[q, x], 1);
        b.feed(ff, 0, &[o, n]);
        b.output("S", x);
        b.output("C", n);
        b.output("Q", q);
        let mut c = Circuit::new();
        let lay = b.clone().build_into(&mut c);
        assert_eq!(lay.conflicts, 0);
        c.check().unwrap();
        assert!(overlaps(&c).is_empty(), "{:?}", overlaps(&c));
        let id = |s: Sig| PinRef { component: lay.ids[s.comp], pin: s.pin };
        let pin = |h: H, k: u8| PinRef { component: lay.ids[h], pin: k };
        let net = |p: PinRef| {
            let mut v = c.net_pins(p);
            v.sort_unstable();
            v
        };
        let mut want = vec![id(a), pin(x.comp, 0), pin(n.comp, 0)];
        want.sort_unstable();
        assert_eq!(net(id(a)), want, "A fans out to both gates");
        let mut want = vec![id(q), pin(o.comp, 0), lay.pins[4].tag_pin()];
        want.sort_unstable();
        assert_eq!(net(id(q)), want, "Q feeds back and leaves");
        assert_eq!(net(id(n)).len(), 3, "carry: AND, flip-flop clock, output tag");
    }

    trait TagPin {
        fn tag_pin(&self) -> PinRef;
    }

    impl TagPin for MacroPin {
        fn tag_pin(&self) -> PinRef {
            PinRef { component: self.tag, pin: 0 }
        }
    }

    #[test]
    fn overlap_checker_sees_shared_segments() {
        let mut c = Circuit::new();
        let sw = |c: &mut Circuit, y| {
            c.add_component(
                ComponentKind::Switch { on: false, mode: bw_core::SwitchMode::Toggle },
                GridPos::new(0, y),
                Rotation::R0,
            )
            .unwrap()
        };
        let led = |c: &mut Circuit, y| c.add_component(ComponentKind::Led, GridPos::new(20, y), Rotation::R0).unwrap();
        let (s1, s2, l1, l2) = (sw(&mut c, 0), sw(&mut c, 10), led(&mut c, 0), led(&mut c, 10));
        let p = |id, pin| PinRef { component: id, pin };
        c.connect(
            p(s1, 0),
            p(l1, 0),
            vec![GridPos::new(10, 1), GridPos::new(10, 5), GridPos::new(15, 5), GridPos::new(15, 1)],
        )
        .unwrap();
        assert!(overlaps(&c).is_empty());
        // A second net running along the same row.
        c.connect(
            p(s2, 0),
            p(l2, 0),
            vec![GridPos::new(8, 11), GridPos::new(8, 5), GridPos::new(17, 5), GridPos::new(17, 11)],
        )
        .unwrap();
        assert!(!overlaps(&c).is_empty());
    }
}
