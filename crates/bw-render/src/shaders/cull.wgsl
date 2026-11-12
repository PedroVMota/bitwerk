// GPU frustum culling: compacts the slots visible in `g.cull_rect` into
// `vis_bodies` / `vis_wires` and writes the arguments of the indirect draws.
//
// Visible bodies are grouped by bucket (= kind code) so each symbol kind can
// be drawn with its own mesh by one indirect draw; the buckets are stored
// back to back, so the whole list is also one contiguous range for the
// far-zoom quad draw. Within a bucket the order is by slot, and buckets are
// in a fixed order, so the draw order is stable (overlapping instances never
// flicker between frames). Three dispatches:
//   cs_count  - per workgroup: visible bodies per bucket, visible wires
//   cs_scan   - one workgroup: exclusive prefix sums over the workgroups of
//               every column, bucket bases and the indirect draw arguments
//   cs_write  - per workgroup: rank within the workgroup, write the indices
// Slot i is handled by invocation i of a (possibly 2-D) dispatch.

struct DrawArgs {
    vertex_count: u32,
    instance_count: u32,
    first_vertex: u32,
    first_instance: u32,
};

// Body buckets (kind codes) and columns (buckets + wires).
const NB: u32 = 10u;
const NCOL: u32 = 11u;
const NONE: u32 = 0xffffu;
// sym_info layout (symbols/atlas.rs): [0, 16) bucket bases, [16, 32) vertex
// counts of the per-bucket symbol draws.
const INFO_VCOUNT: u32 = 16u;

@group(1) @binding(0) var<storage, read> c_bodies: array<Body>;
@group(1) @binding(1) var<storage, read> c_wires: array<Wire>;
@group(1) @binding(2) var<storage, read_write> c_vis_bodies: array<u32>;
@group(1) @binding(3) var<storage, read_write> c_vis_wires: array<u32>;
// [0] all bodies as quads, [1] wires, [2 + b] symbols of bucket b.
@group(1) @binding(4) var<storage, read_write> c_args: array<DrawArgs, 12>;
// Column-major: [col * cull_groups + group] counts, replaced by offsets.
@group(1) @binding(5) var<storage, read_write> c_groups: array<u32>;
@group(1) @binding(6) var<storage, read_write> c_info: array<u32>;

const WG: u32 = 256u;

var<workgroup> sh_cnt: array<atomic<u32>, 16>;
var<workgroup> sh_tag: array<u32, 256>;
var<workgroup> sh_scan: array<u32, 256>;
var<workgroup> sh_tot: array<u32, 16>;

fn overlaps(lo: vec2<f32>, hi: vec2<f32>) -> bool {
    return all(hi >= g.cull_rect.xy) && all(lo <= g.cull_rect.zw);
}

// Bucket of a visible body, or NONE.
fn body_bucket(i: u32) -> u32 {
    if (i >= g.body_count) {
        return NONE;
    }
    let b = c_bodies[i];
    if (b.kind == HIDDEN_KIND || !overlaps(b.min, b.min + b.size)) {
        return NONE;
    }
    return min(b.kind & 0xffu, NB - 1u);
}

fn wire_visible(i: u32) -> u32 {
    if (i >= g.wire_count) {
        return 0u;
    }
    let w = c_wires[i];
    if ((w.flags & 1u) != 0u) {
        return 0u;
    }
    // The CPU widens cull_rect by the wire half-width.
    return u32(overlaps(min(w.a, w.b), max(w.a, w.b)));
}

@compute @workgroup_size(256)
fn cs_count(
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
) {
    let group = wid.x + wid.y * nwg.x;
    let i = group * WG + lid;
    if (lid < 16u) {
        atomicStore(&sh_cnt[lid], 0u);
    }
    workgroupBarrier();
    let b = body_bucket(i);
    if (b != NONE) {
        atomicAdd(&sh_cnt[b], 1u);
    }
    if (wire_visible(i) == 1u) {
        atomicAdd(&sh_cnt[NB], 1u);
    }
    workgroupBarrier();
    if (lid < NCOL && group < g.cull_groups) {
        c_groups[lid * g.cull_groups + group] = atomicLoad(&sh_cnt[lid]);
    }
}

// Inclusive Hillis-Steele scan of `sh_scan` across the workgroup.
fn scan_shared(lid: u32) {
    for (var off = 1u; off < WG; off = off << 1u) {
        var v = 0u;
        if (lid >= off) {
            v = sh_scan[lid - off];
        }
        workgroupBarrier();
        sh_scan[lid] = sh_scan[lid] + v;
        workgroupBarrier();
    }
}

@compute @workgroup_size(256)
fn cs_scan(@builtin(local_invocation_index) lid: u32) {
    let n = g.cull_groups;
    let per = (n + WG - 1u) / WG;
    let start = min(lid * per, n);
    let end = min(start + per, n);
    for (var col = 0u; col < NCOL; col++) {
        let base = col * n;
        var sum = 0u;
        for (var k = start; k < end; k++) {
            sum += c_groups[base + k];
        }
        sh_scan[lid] = sum;
        workgroupBarrier();
        scan_shared(lid);
        var run = sh_scan[lid] - sum;
        for (var k = start; k < end; k++) {
            let c = c_groups[base + k];
            c_groups[base + k] = run;
            run += c;
        }
        if (lid == WG - 1u) {
            sh_tot[col] = sh_scan[lid];
        }
        workgroupBarrier();
    }
    if (lid == 0u) {
        var total = 0u;
        for (var b = 0u; b < NB; b++) {
            c_info[b] = total;
            c_args[2u + b] = DrawArgs(c_info[INFO_VCOUNT + b], sh_tot[b], b << 16u, 0u);
            total += sh_tot[b];
        }
        c_args[0] = DrawArgs(6u, total, 0u, 0u);
        c_args[1] = DrawArgs(6u, sh_tot[NB], 0u, 0u);
    }
}

@compute @workgroup_size(256)
fn cs_write(
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
    @builtin(num_workgroups) nwg: vec3<u32>,
) {
    let group = wid.x + wid.y * nwg.x;
    let i = group * WG + lid;
    let b = body_bucket(i);
    let wv = wire_visible(i);
    sh_tag[lid] = b | (wv << 16u);
    workgroupBarrier();
    if (group >= g.cull_groups || (b == NONE && wv == 0u)) {
        return;
    }
    // Rank among the earlier invocations of this workgroup (keeps slot order).
    var rb = 0u;
    var rw = 0u;
    for (var j = 0u; j < lid; j++) {
        let t = sh_tag[j];
        rb += u32((t & 0xffffu) == b);
        rw += t >> 16u;
    }
    let n = g.cull_groups;
    if (b != NONE) {
        c_vis_bodies[c_info[b] + c_groups[b * n + group] + rb] = i;
    }
    if (wv == 1u) {
        c_vis_wires[c_groups[NB * n + group] + rw] = i;
    }
}
