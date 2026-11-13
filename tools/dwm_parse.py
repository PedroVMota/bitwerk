#!/usr/bin/env python3
"""Defensive reference parser for Digital Works (.dwm / .dwt) files.

Format recovered from DigitalWorks.exe 3.0.5.0 (static disassembly of the
TSerialize.Read overrides, slot 5 of each VMT) and verified against all
sample files.  See DWM-FORMAT.md.

Design:
  * no recursion: every class reader is a generator that *yields* when it
    needs a nested object; a driver loop keeps an explicit stack.  This
    mirrors the recommended Rust design (explicit stack machine).
  * hard limits on file size, object count, stack depth, string length,
    memory word count, list length.
  * never trusts lengths: every read is bounds-checked.

Usage:
  python3 -I dwm_parse.py [--json OUTDIR] FILE_OR_DIR...

Research tool kept for reference (see docs/dwm/README.md); the application
uses the Rust importer in crates/bw-io/src/dwm/.
"""
import json
import math
import os
import struct
import sys
from collections import Counter, defaultdict

# ---------------------------------------------------------------- limits
MAX_FILE = 64 * 1024 * 1024
MAX_OBJECTS = 2_000_000
MAX_STACK = 200_000          # nesting of objects being read (DFS depth)
MAX_STR = 1 << 20
MAX_LIST = 1_000_000
MAX_MEM_WORDS = 1 << 20      # memory devices: words of contents
MAGIC_LO = 0xC4DD
DWT_MAGIC_LO = 0xED19        # templates use a different 16-bit id
SUPPORTED_VERSIONS = {0x14, 0x1E, 0x22}

# Delphi TValueType tags that occur in the files
VA_NULL, VA_LIST, VA_INT8, VA_INT16, VA_INT32 = 0, 1, 2, 3, 4
VA_STRING, VA_FALSE, VA_TRUE, VA_LSTRING = 6, 8, 9, 0x0C

# object-reference kinds (DATA constants at 0x4A2BA4..0x4A2BB0)
OBJ_NEW_CLASS, OBJ_KNOWN_CLASS, OBJ_BACKREF, OBJ_NIL = 0, 1, 2, 15

# class -> parent (from the VMT chain; only TSerialize descendants)
PARENT = {
    "T2AndGate": "TAndGate", "T2NandGate": "TNandGate", "T2NorGate": "TNorGate",
    "T2OrGate": "TOrGate", "T2XnorGate": "TXnorGate", "T2XorGate": "TXorGate",
    "T3AndGate": "TAndGate", "T3NandGate": "TNandGate", "T3NorGate": "TNorGate",
    "T3OrGate": "TOrGate", "T4AndGate": "TAndGate", "T4NandGate": "TNandGate",
    "T4NorGate": "TNorGate", "T4OrGate": "TOrGate", "TAndGate": "TGateDevice",
    "TBitGenerator": "TSeqInputDevice", "TClockDevice": "TSeqInputDevice",
    "TClockedFlipFlop": "TFlipFlop", "TDFlipFlop": "TClockedFlipFlop",
    "TDevice": "TGraphicList", "TFlipFlop": "TDevice", "TFloatPoint": "TSerialize",
    "TGateDevice": "TDevice", "TGraphicBox": "TPolygon", "TGraphicList": "TGraphicObject",
    "TGraphicObject": "TSerialize", "TGraphicText": "TPolygon", "TGround": "TInputDevice",
    "THistoryDevice": "TTextDevice", "TInputDevice": "THistoryDevice",
    "TInteractiveInput": "TInputDevice", "TInteractiveSwitch": "TTextDevice",
    "TJKFlipFlop": "TClockedFlipFlop", "TLEDDevice": "TOutputDevice",
    "TLEDSegmentDevice": "TOutputDevice", "TMacro": "TDevice", "TMacroPin": "TPin",
    "TMemoryDevice": "TDevice", "TMessageBox": "TGraphicBox", "TNandGate": "TGateDevice",
    "TNorGate": "TGateDevice", "TNotGate": "TGateDevice", "TNumericDevice": "TOutputDevice",
    "TOrGate": "TGateDevice", "TOutputDevice": "THistoryDevice", "TPin": "TPolygon",
    "TPolygon": "TSegment", "TPolyline": "TSegment", "TRSFlipFlop": "TFlipFlop",
    "TSegment": "TGraphicObject", "TSeqInputDevice": "TInputDevice",
    "TSerializeList": "TSerialize", "TSolder": "TPolygon", "TSupply": "TInputDevice",
    "TTagDevice": "TDevice", "TTextDevice": "TDevice", "TTriState": "TDevice",
    "TWire": "TPolyline", "TXnorGate": "TGateDevice", "TXorGate": "TGateDevice",
}


class DwmError(Exception):
    pass


class Reader:
    def __init__(self, data, version=None):
        self.b = data
        self.p = 0
        self.version = version

    def need(self, n):
        if n < 0 or self.p + n > len(self.b):
            raise DwmError(f"unexpected EOF at 0x{self.p:x} (need {n})")

    def u8(self):
        self.need(1)
        v = self.b[self.p]
        self.p += 1
        return v

    def peek(self):
        self.need(1)
        return self.b[self.p]

    def read_int(self):
        t = self.u8()
        if t == VA_INT8:
            self.need(1); v = struct.unpack_from("<b", self.b, self.p)[0]; self.p += 1
        elif t == VA_INT16:
            self.need(2); v = struct.unpack_from("<h", self.b, self.p)[0]; self.p += 2
        elif t == VA_INT32:
            self.need(4); v = struct.unpack_from("<i", self.b, self.p)[0]; self.p += 4
        else:
            raise DwmError(f"expected integer tag at 0x{self.p-1:x}, got 0x{t:02x}")
        return v

    def read_bool(self):
        t = self.u8()
        if t == VA_FALSE:
            return False
        if t == VA_TRUE:
            return True
        raise DwmError(f"expected boolean tag at 0x{self.p-1:x}, got 0x{t:02x}")

    def read_str(self):
        t = self.u8()
        if t == VA_STRING:
            n = self.u8()
        elif t == VA_LSTRING:
            self.need(4)
            n = struct.unpack_from("<i", self.b, self.p)[0]
            self.p += 4
            if n < 0 or n > MAX_STR:
                raise DwmError(f"bad long-string length {n} at 0x{self.p-4:x}")
        else:
            raise DwmError(f"expected string tag at 0x{self.p-1:x}, got 0x{t:02x}")
        self.need(n)
        s = self.b[self.p:self.p + n].decode("cp1252", errors="replace")
        self.p += n
        return s

    def read_single(self):
        """Raw IEEE-754 float32, little endian, *no* tag byte."""
        self.need(4)
        v = struct.unpack_from("<f", self.b, self.p)[0]
        self.p += 4
        if not math.isfinite(v):
            raise DwmError(f"non-finite single at 0x{self.p-4:x}")
        return v

    def list_begin(self):
        t = self.u8()
        if t != VA_LIST:
            raise DwmError(f"expected list-begin at 0x{self.p-1:x}, got 0x{t:02x}")

    def end_of_list(self):
        return self.peek() == VA_NULL

    def list_end(self):
        t = self.u8()
        if t != VA_NULL:
            raise DwmError(f"expected list-end at 0x{self.p-1:x}, got 0x{t:02x}")


OBJ = object()   # sentinel yielded by class readers to request a nested object


# ------------------------------------------------------------ class readers
# Each reader is a generator: f(r, o) where o is the object's field dict.
# `x = yield OBJ` reads one nested object and returns its id (or None).

def rd_serializelist(r, o):
    r.list_begin()
    items = []
    while not r.end_of_list():
        if len(items) >= MAX_LIST:
            raise DwmError("list too long")
        items.append((yield OBJ))
    r.list_end()
    o["items"] = items


def rd_floatpoint(r, o):
    o["x"] = r.read_single()
    o["y"] = r.read_single()
    return
    yield


def rd_graphiclist(r, o):            # TGraphicList.Read  @454E28
    o["children"] = yield OBJ


def rd_segment(r, o):                # TSegment.Read      @4543EC
    o["pen_color"] = r.read_int()
    o["pen_width"] = r.read_int()
    o["points"] = yield OBJ          # TSerializeList of TFloatPoint (relative)


def rd_polygon(r, o):                # TPolygon.Read      @454AD8
    yield from rd_segment(r, o)
    o["brush_color"] = r.read_int()


def rd_pin(r, o):                    # TPin.Read          @455A44
    yield from rd_polygon(r, o)
    o["pin_kind"] = r.read_int()     # see spec: 1/2 (direction)
    o["wire"] = yield OBJ            # connected TWire or nil


def rd_macropin(r, o):               # TMacroPin.Read     @455BB8
    yield from rd_pin(r, o)
    o["partner"] = yield OBJ         # the paired TMacroPin (outer <-> inner)


def rd_wire(r, o):                   # TWire.Read         @455D04 (TPolyline->TSegment)
    yield from rd_segment(r, o)
    o["unk_int_3c"] = r.read_int()
    o["end1"] = yield OBJ            # TPin / TSolder / nil
    o["end2"] = yield OBJ


def rd_solder(r, o):                 # TSolder.Read       @45666C (does NOT call TPolygon)
    o["x"] = r.read_single()
    o["y"] = r.read_single()
    o["wire1"] = yield OBJ
    o["wire2"] = yield OBJ
    o["wire3"] = yield OBJ


def rd_graphictext(r, o):            # TGraphicText.Read  @456B00
    yield from rd_polygon(r, o)
    o["font_size"] = r.read_int()
    o["font_color"] = r.read_int()
    o["flag_8d"] = r.read_bool()
    o["font_weight"] = r.read_int()
    o["angle"] = r.read_single()
    o["text"] = r.read_str()
    o["flag_94"] = r.read_bool()
    o["has_overbar"] = r.read_bool()
    if o["has_overbar"]:
        o["overbar"] = yield OBJ


def rd_device(r, o):                 # TDevice.Read       @457600
    o["has_graphics"] = r.read_bool()
    if o["has_graphics"]:
        yield from rd_graphiclist(r, o)
    o["interactive"] = r.read_bool()
    o["flag_1c"] = r.read_bool()
    o["pins"] = yield OBJ            # TSerializeList of TPin
    if not o["has_graphics"]:
        o["x"] = r.read_single()
        o["y"] = r.read_single()


def rd_tagdevice(r, o):              # TTagDevice.Read    @461F14
    yield from rd_device(r, o)
    o["text_pos"] = r.read_int()
    o["direction"] = r.read_int()
    o["macro_pin"] = yield OBJ       # inner TMacroPin paired with an outer pin


def rd_macro(r, o):                  # TMacro.Read        @46264C
    yield from rd_device(r, o)
    o["datasheet"] = r.read_str()
    o["unk_int_10"] = r.read_int()       # converted with IntToStr into a string field
    o["embedded_38"] = r.read_bool()
    o["embedded_3a"] = r.read_bool()
    o["devices"] = yield OBJ
    o["tags"] = yield OBJ
    o["inputs"] = yield OBJ
    o["outputs"] = yield OBJ
    o["solders"] = yield OBJ
    o["wires"] = yield OBJ


def rd_textdevice(r, o):             # TTextDevice.Read   @457AE0
    yield from rd_device(r, o)
    o["has_label"] = r.read_bool()
    if o["has_label"]:
        o["text_pos"] = r.read_int()
        o["name_label"] = yield OBJ  # TGraphicText name label


def rd_history(r, o):                # THistoryDevice.Read @457F74
    yield from rd_textdevice(r, o)
    o["logic_history"] = r.read_bool()


def rd_iswitch(r, o):                # TInteractiveSwitch.Read @48127C
    yield from rd_textdevice(r, o)
    o["switch_type"] = r.read_int()
    o["flag_3c"] = r.read_bool()


def rd_output(r, o):                 # TOutputDevice.Read @45F6E8
    yield from rd_history(r, o)
    o["color"] = r.read_int()


def rd_iinput(r, o):                 # TInteractiveInput.Read @4815EC
    yield from rd_history(r, o)
    o["pulse"] = r.read_bool()
    o["state"] = r.read_int()


def rd_ledseg(r, o):                 # TLEDSegmentDevice.Read @45FEE8
    yield from rd_output(r, o)
    if r.version in (0x1E, 0x22):
        o["common_cathode"] = r.read_bool()


def rd_numeric(r, o):                # TNumericDevice.Read @460420
    yield from rd_output(r, o)
    o["input_lines"] = r.read_int()
    o["unk_58"] = r.read_int()
    o["unk_5c"] = r.read_int()
    o["unk_color_60"] = r.read_int()
    o["flag_64"] = r.read_bool()


def rd_seqinput(r, o):               # TSeqInputDevice.Read @45EE80
    yield from rd_history(r, o)
    o["sequence"] = r.read_str()
    o["color"] = r.read_int()


def rd_memory(r, o):                 # TMemoryDevice.Read @48BDA4
    yield from rd_device(r, o)
    o["data_bits"] = r.read_int()
    o["address_bits"] = r.read_int()
    o["is_ram"] = r.read_bool()
    o["unused_bool"] = r.read_bool()
    n = r.read_int()
    if n < 0 or n > MAX_MEM_WORDS:
        raise DwmError(f"bad memory word count {n}")
    o["word_count"] = n
    o["contents"] = [r.read_int() & 0xFFFF for _ in range(n)]


READERS = {
    "TSerializeList": rd_serializelist, "TFloatPoint": rd_floatpoint,
    "TGraphicList": rd_graphiclist, "TSegment": rd_segment, "TPolygon": rd_polygon,
    "TPin": rd_pin, "TMacroPin": rd_macropin, "TWire": rd_wire, "TSolder": rd_solder,
    "TGraphicText": rd_graphictext, "TDevice": rd_device, "TTagDevice": rd_tagdevice,
    "TMacro": rd_macro, "TTextDevice": rd_textdevice, "THistoryDevice": rd_history,
    "TInteractiveSwitch": rd_iswitch, "TOutputDevice": rd_output,
    "TInteractiveInput": rd_iinput, "TLEDSegmentDevice": rd_ledseg,
    "TNumericDevice": rd_numeric, "TSeqInputDevice": rd_seqinput,
    "TMemoryDevice": rd_memory,
}


def reader_for(cls):
    c = cls
    seen = 0
    while c is not None and seen < 32:
        if c in READERS:
            return READERS[c]
        c = PARENT.get(c)
        seen += 1
    raise DwmError(f"unknown/unsupported class {cls!r}")


# ---------------------------------------------------------- object driver
def read_object_graph(r, classes=None, objects=None):
    """Read one object (and all nested ones) iteratively.  Returns
    (root_id, objects) where objects[i] = {"cls":..., **fields}."""
    classes = [] if classes is None else classes
    objects = [] if objects is None else objects
    stack = []        # list of (generator, object id)
    result = None     # value to send into the top generator

    def start_object():
        """Read the object header.  Returns ('ref', id|None) or ('new', gen)."""
        kind = r.read_int()
        if kind == OBJ_NIL:
            return ("ref", None)
        if kind == OBJ_BACKREF:
            idx = r.read_int()
            if not 0 <= idx < len(objects):
                raise DwmError(f"back-reference to unknown object {idx} at 0x{r.p:x}")
            return ("ref", idx)
        if kind == OBJ_KNOWN_CLASS:
            ci = r.read_int()
            if not 0 <= ci < len(classes):
                raise DwmError(f"unknown class index {ci} at 0x{r.p:x}")
            cls = classes[ci]
        elif kind == OBJ_NEW_CLASS:
            cls = r.read_str()
            if not cls.isidentifier() or len(cls) > 64:
                raise DwmError(f"bad class name {cls!r}")
            reader_for(cls)  # validate before registering
            classes.append(cls)
        else:
            raise DwmError(f"bad object kind {kind} at 0x{r.p:x}")
        if len(objects) >= MAX_OBJECTS:
            raise DwmError("too many objects")
        o = {"id": len(objects), "cls": cls}
        objects.append(o)              # index assigned BEFORE fields are read
        return ("new", reader_for(cls)(r, o), o)

    first = start_object()
    if first[0] == "ref":
        return first[1], objects, classes
    root = first[2]["id"]
    stack.append((first[1], root))
    result = None
    while stack:
        gen = stack[-1][0]
        try:
            gen.send(result)
        except StopIteration:
            result = stack.pop()[1]     # finished child id goes to its parent
            continue
        # gen requested a nested object
        nxt = start_object()
        if nxt[0] == "ref":
            result = nxt[1]
            continue
        if len(stack) >= MAX_STACK:
            raise DwmError("object nesting too deep")
        stack.append((nxt[1], nxt[2]["id"]))
        r.max_depth = max(getattr(r, "max_depth", 0), len(stack))
        result = None
    return root, objects, classes


def parse_bytes(data, name="<mem>"):
    if len(data) > MAX_FILE:
        raise DwmError("file too large")
    r = Reader(data)
    magic = r.read_int()
    lo, ver = magic & 0xFFFF, (magic >> 16) & 0xFF
    kind = "dwm" if lo == MAGIC_LO else "dwt" if lo == DWT_MAGIC_LO else None
    if kind is None or (magic >> 24) != 0:
        raise DwmError(f"bad magic 0x{magic:08x}")
    if kind == "dwm" and ver not in SUPPORTED_VERSIONS:
        raise DwmError(f"unsupported version 0x{ver:02x}")
    r.version = ver
    hdr = {"magic": f"0x{magic:08x}", "version": ver, "kind": kind}
    if kind == "dwt":
        pass  # templates: object graph follows directly (see spec)
    elif ver == 0x1E:
        hdr["bool"] = r.read_bool()
        hdr["info"] = r.read_str()          # stored to form field +0x200
        hdr["str2"] = r.read_str()
        hdr["int"] = r.read_int()
    elif ver == 0x22:
        props = []
        r.list_begin()
        while not r.end_of_list():
            if len(props) > 1000:
                raise DwmError("too many header properties")
            pk = r.read_int()
            pid = r.read_int()
            if pk == 1:
                val = r.read_str()
            elif pk == 2:
                val = r.read_int()
            elif pk == 3:
                val = r.read_bool()
            else:
                raise DwmError(f"bad header property kind {pk}")
            props.append({"kind": pk, "id": pid, "value": val})
        r.list_end()
        hdr["props"] = props
    root, objects, classes = read_object_graph(r)
    roots = [root]
    if kind == "dwt":
        # a template is a sequence of top-level objects sharing one class
        # table and one object table: [graphics TSerializeList, TGraphicList of
        # TMacroPin (the DIL pin positions)]
        while r.p < len(data) and len(roots) < 16:
            nxt, objects, classes = read_object_graph(r, classes, objects)
            roots.append(nxt)
    trailing = len(data) - r.p
    return {"file": name, "header": hdr, "root": root, "roots": roots, "classes": classes,
            "objects": objects, "trailing_bytes": trailing,
            "max_nesting": getattr(r, "max_depth", 1)}


# ------------------------------------------------------------ JSON model
def build_model(raw):
    """Type-checked wrapper: any reference that points at an object of the
    wrong class (corrupt or hostile file) becomes a DwmError."""
    try:
        return _build_model(raw)
    except (KeyError, TypeError, IndexError, AttributeError) as e:
        raise DwmError(f"inconsistent object graph: {type(e).__name__} {e}") from None


def _build_model(raw):
    """Turn the raw object table into a circuit model:
    components (with pins + nets), wires, solders, texts, nested macros,
    memory contents.  Net ids are union-find roots over pin/solder ids,
    local to each macro level; macro boundaries are described by
    `ports` (outer pin index <-> inner tag pin)."""
    objs = raw["objects"]
    root = raw["root"]
    kind = raw["header"]["kind"]

    def o(i):
        return None if i is None else objs[i]

    def items(i):
        x = o(i)
        if x is None:
            return []
        if x["cls"] not in ("TGraphicList", "TSerializeList"):
            raise DwmError(f"object {i} ({x['cls']}) is not a list")
        if x["cls"] == "TGraphicList":
            x = o(x["children"])
            if x is None:
                return []
        return [j for j in x.get("items", []) if j is not None]

    def pts(i):
        return [[objs[j]["x"], objs[j]["y"]] for j in items(i) if objs[j]["cls"] == "TFloatPoint"]

    def graphic(i):
        g = objs[i]
        rec = {"cls": g["cls"], "points": pts(g["points"]), "pen_color": g["pen_color"]}
        if "brush_color" in g:
            rec["brush_color"] = g["brush_color"]
        if g["cls"] == "TGraphicText":
            rec.update(text=g["text"], font_size=g["font_size"], font_color=g["font_color"],
                       bold=g["font_weight"] >= 700, angle=g["angle"],
                       overbar=g.get("overbar") is not None)
        return rec

    def pin(i):
        p = objs[i]
        if p["cls"] not in ("TPin", "TMacroPin"):
            raise DwmError(f"object {i} ({p['cls']}) is not a pin")
        return {"id": i, "cls": p["cls"], "kind": p["pin_kind"], "outline": pts(p["points"]),
                "wire": p["wire"], "partner": p.get("partner")}

    SKIP = {"id", "cls", "points", "pins", "children", "contents", "name_label",
            "devices", "tags", "inputs", "outputs", "solders", "wires"}

    def circuit(mid, depth):
        if depth > 64:
            raise DwmError("macro nesting too deep")
        m = objs[mid]
        parent = {}

        def find(a):
            parent.setdefault(a, a)
            while parent[a] != a:
                parent[a] = parent[parent[a]]
                a = parent[a]
            return a

        def union(a, b):
            ra, rb = find(a), find(b)
            if ra != rb:
                parent[ra] = rb

        comps, texts = [], []
        for cid in items(m["devices"]) + items(m["inputs"]) + items(m["outputs"]):
            c = objs[cid]
            if c["cls"] == "TGraphicText":
                texts.append(graphic(cid))
                continue
            comp = {"id": cid, "cls": c["cls"], "x": c.get("x"), "y": c.get("y"),
                    "pins": [pin(p) for p in items(c["pins"])],
                    "props": {k: v for k, v in c.items() if k not in SKIP}}
            if c.get("has_graphics"):
                comp["graphics"] = [graphic(g) for g in items(c["children"])]
            if c.get("name_label") is not None:
                comp["label"] = objs[c["name_label"]]["text"]
            if c["cls"] == "TMemoryDevice":
                comp["contents"] = c["contents"]
            if c["cls"] == "TMacro":
                comp["macro"] = circuit(cid, depth + 1)
            for p in comp["pins"]:
                find(p["id"])
            comps.append(comp)
        wires = []
        for wid in items(m["wires"]):
            w = objs[wid]
            if w["cls"] != "TWire":
                raise DwmError(f"object {wid} ({w['cls']}) is not a wire")
            wires.append({"id": wid, "points": pts(w["points"]), "pen_color": w["pen_color"],
                          "end1": w["end1"], "end2": w["end2"]})
            for e in (w["end1"], w["end2"]):
                if e is not None:
                    union(wid, e)
        solders = []
        for sid in items(m["solders"]):
            s = objs[sid]
            if s["cls"] != "TSolder":
                raise DwmError(f"object {sid} ({s['cls']}) is not a solder")
            ws = [s["wire1"], s["wire2"], s["wire3"]]
            solders.append({"id": sid, "x": s["x"], "y": s["y"], "wires": ws})
            for w in ws:
                if w is not None:
                    union(sid, w)
        tags = []
        ext_index = {pid: k + 1 for k, pid in enumerate(items(m["pins"]))}
        for tid in items(m["tags"]):
            t = objs[tid]
            tpins = items(t["pins"])
            inner = objs[t["macro_pin"]] if t["macro_pin"] is not None else None
            outer = inner.get("partner") if inner else None
            for p in tpins:
                find(p)
            tags.append({"id": tid, "pin": tpins[0] if tpins else None,
                         "direction": t["direction"], "text_pos": t["text_pos"],
                         "text": next((objs[g]["text"] for g in items(t["children"])
                                       if objs[g]["cls"] == "TGraphicText"), None),
                         "outer_pin_index": ext_index.get(outer)})
        nets = {}
        for comp in comps:
            for p in comp["pins"]:
                p["net"] = find(p["id"])
                nets.setdefault(p["net"], []).append([comp["id"], p["id"]])
        for t in tags:
            if t["pin"] is not None:
                t["net"] = find(t["pin"])
                nets.setdefault(t["net"], []).append(["tag", t["id"]])
        return {"datasheet": m.get("datasheet"),
                "graphics": [graphic(g) for g in items(m["children"])],
                "ports": [pin(p) for p in items(m["pins"])],
                "components": comps, "wires": wires, "solders": solders,
                "tags": tags, "texts": texts,
                "nets": {str(k): v for k, v in nets.items()}}

    out = {"file": raw["file"], "header": raw["header"]}
    if kind == "dwt":
        out["template"] = {
            "graphics": [graphic(g) for g in items(raw["roots"][0])],
            "pins": [pin(p) for r in raw["roots"][1:] for p in items(r)],
        }
    elif root is not None and objs[root]["cls"] == "TMacro":
        out["circuit"] = circuit(root, 0)
    return out


# ---------------------------------------------------------------- CLI
def iter_files(paths):
    for p in paths:
        if os.path.isdir(p):
            for dp, dn, fn in os.walk(p, followlinks=False):
                dn.sort()
                for f in sorted(fn):
                    if f.lower().endswith((".dwm", ".dwt")):
                        yield os.path.join(dp, f)
        else:
            yield p


def main(argv):
    outdir = None
    if len(argv) >= 2 and argv[0] == "--json":
        outdir = argv[1]
        argv = argv[2:]
        os.makedirs(outdir, exist_ok=True)
    ok, failed = 0, []
    hist = Counter()
    versions = Counter()
    for path in iter_files(argv):
        try:
            if os.path.getsize(path) > MAX_FILE:
                raise DwmError("file too large")
            with open(path, "rb") as f:
                data = f.read(MAX_FILE + 1)
            raw = parse_bytes(data, path)
            if raw["trailing_bytes"]:
                raise DwmError(f"{raw['trailing_bytes']} trailing bytes")
            model = build_model(raw)
            ok += 1
            versions[(raw["header"]["kind"], raw["header"]["version"])] += 1
            for o in raw["objects"]:
                hist[o["cls"]] += 1
            if outdir:
                safe = path.replace(os.sep, "_").lstrip("_")[-150:]
                with open(os.path.join(outdir, safe + ".json"), "w") as f:
                    json.dump({"raw": raw, "model": model}, f)
        except (DwmError, RecursionError, UnicodeError, struct.error) as e:
            failed.append((path, str(e)))
    print(f"files OK: {ok}  failed: {len(failed)}")
    for p, e in failed:
        print(f"  FAIL {p}: {e}")
    print("versions:", dict(versions))
    print("class histogram:")
    for c, n in hist.most_common():
        print(f"  {c:22s} {n}")
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
