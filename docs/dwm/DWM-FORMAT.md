# Digital Works `.dwm` / `.dwt` / `.map` file format

Status: **complete for every file in the official package**. 495/495 `.dwm` and 8/8 `.dwt` parse with zero trailing bytes. Three sample circuits were also simulated correctly after import (see `validation.txt`).

## How this was established

- **Disassembly** of `DigitalWorks.exe` 3.0.5.0 (Delphi, i386) with `objdump`, read only as bytes; nothing was executed.
- **Delphi VMTs.** Each class's VMT was located through its `vmtSelfPtr` (VMT−76) and `vmtClassName` (VMT−44). The parent chain comes from `vmtParent`.
- **Read methods.** Every class serialises itself through virtual methods of a custom base class `TSerialize` (a `TPersistent` descendant): slot 4 = `Write(Writer)` and **slot 5 = `Read(Reader)`**. Each slot-5 override was disassembled. The call sequences to `TReader.ReadInteger` / `ReadBoolean` / `ReadString` / `ReadListBegin` / `EndOfList` / `ReadListEnd` and to two custom helpers (`ReadObject`, `ReadSingleRaw`) give the exact field order. Section 4 gives the code addresses for each class.
- **Field meanings** come from value statistics over the corpus (`coverage.md`), the help file, the pin geometry, and IC macros whose tag numbers match datasheet pin numbers.
- **No checksums and no compression.** Integers are little-endian.

## 1. Overall structure

The format is **not** a Delphi DFM/`TPF0` stream. It reuses only Delphi `TReader`/`TWriter` *value encodings*, with no property names (**fields are positional**), plus a custom object-graph layer.

```
file      := magic header object
magic     := Integer                ; low 16 bits = 0xC4DD (.dwm) / 0xED19 (.dwt)
                                    ; bits 16..23 = format version, bits 24..31 = 0
header    := (depends on version, §2)
object    := object reference (§3); the root object is a TMacro
```

The writer (0x452C78) emits `Integer((version << 16) | 0xC4DD)`. The reader (0x452F20) checks the low word and extracts the version byte. The loader (0x484A4E) rejects versions > 0x22 with "You need a later version of Digital Works…".

| version byte | magic (hex bytes as in file) | files | era |
|---|---|---|---|
| 0x14 (20) | `04 dd c4 14 00` | 3 | DW 2.x |
| 0x1E (30) | `04 dd c4 1e 00` | 51 | DW 3.0 early |
| 0x22 (34) | `04 dd c4 22 00` | 441 | DW 3.x (current) |

DW 3 reads all three versions. Class layouts are identical across versions except `TLEDSegmentDevice` (§4).

## 2. Header (between magic and root object)

- **v20**: nothing. The root object follows the magic directly.
- **v30**: `Boolean` (unknown, always False), `String` macro name / description (form field +0x200, the "Name Macro" text), `String` datasheet path (the v34 property 101; empty in 38 of the 51 files, e.g. `'\motorola\sn74ls90.pdf'` in the others), `Integer` (unknown, always 0).
- **v34**: a property list (reader at 0x452F84):
  ```
  ListBegin { Integer kind; Integer id; value } ListEnd
      kind 1 -> String, kind 2 -> Integer, kind 3 -> Boolean
  ```
  The ids seen are:

  | id | kind | meaning |
  |---|---|---|
  | 100 | String | macro name / description ("Name Macro"), e.g. `'Quadruple 2 Input NAND Gates'` |
  | 101 | String | datasheet path, e.g. `'\philips\74hc_hct00_cnv_2.pdf'` (relative to the Datasheets dir) |
  | 102 | String `'0'` or Integer 0 | unknown |
  | 200 | Integer 0/1 | unknown (1 in 370/387 files) |
  | 202 | Integer 0/1 | unknown |

  Unknown ids should be ignored, since the writer is generic. Example from `Counter.dwm`:
  `01 | 02 01 02 64 06 17 "Digital Counter Circuit" | 02 01 02 65 06 00 | 02 01 02 66 06 01 "0" | 02 02 03 ca 00 02 00 | 02 02 03 c8 00 02 01 | 00`
- **.dwt** (magic `0x0014ED19`): no header. The body is a *sequence* of top-level objects that share one class table and one object table, read until EOF (§7).

## 3. Value encodings

Every value except "raw singles" starts with a 1-byte Delphi `TValueType` tag.

| tag | Delphi | payload | used by |
|---|---|---|---|
| 0x00 | vaNull | — | list terminator |
| 0x01 | vaList | — | list start |
| 0x02 | vaInt8 | i8 | `ReadInteger` |
| 0x03 | vaInt16 | i16 LE | `ReadInteger` |
| 0x04 | vaInt32 | i32 LE | `ReadInteger` |
| 0x06 | vaString | u8 len + bytes (cp1252) | `ReadString` |
| 0x0C | vaLString | i32 len + bytes | `ReadString` (accept it; not seen in the corpus) |
| 0x08 / 0x09 | vaFalse / vaTrue | — | `ReadBoolean` |

The writer always uses the smallest integer form, but readers must accept any of 0x02–0x04 for every Integer. Other Delphi tags (extended, set, binary, collection, wide string) **never occur**. Treat them as errors.

**Raw single** (`ReadSingleRaw`, 0x452DE4 = `TReader.Read(buf, 4)`): 4 bytes IEEE-754 float32 LE, **no tag**. Used for every coordinate.

### Object references (`ReadObject` 0x452E04 / `WriteObject` 0x452B50)

```
object := Integer kind, then
   kind 0  (new class)   : String className      -> append to class table, create instance
   kind 1  (known class) : Integer classIndex     -> create instance of classTable[classIndex]
   kind 2  (back-ref)    : Integer objectIndex    -> existing object objectTable[objectIndex]
   kind 15 (nil)         : —                      -> null
```

The constants 0/1/2/15 live at 0x4A2BA4..0x4A2BB0. For kinds 0 and 1, the new instance is appended to the **object table before its fields are read** (index = running count from 0, over all objects including TFloatPoints and lists). Then `instance.Read(reader)` runs. A back-reference may therefore point to an ancestor that is still being read; there are never forward references.

Because of this, the stream is a DFS over the whole object graph. Wires, solders and pins are first serialised wherever they are first reached (often inside a pin of a device) and are later referenced by index from the owning macro's lists.

Example from the XOR sample: `02 00 06 06 "TMacro"` = new class TMacro (class 0, object 0). `02 01 02 03 <8 raw bytes>` = new object of class 3 (TFloatPoint) with x,y. `02 02 02 1b` = reference to object 27.

## 4. Class layouts (Read order)

Notation: `Int` = ReadInteger, `Bool` = ReadBoolean, `Str` = ReadString, `F32` = raw single, `Obj` = object reference. "⊃ P" means "first read everything of parent P". Hex = address of the Read method.

### Infrastructure

- **TSerializeList** (0x452880): `ListBegin; while !EndOfList: items += Obj; ListEnd`
- **TFloatPoint** (0x4532B4): `x F32, y F32`
- **TGraphicList** (0x454E28): `children Obj` (TSerializeList)
- **TGraphicObject**: abstract, no own fields. **TSerialize**: none.

### Graphics

- **TSegment** (0x4543EC): `pen_color Int` (TColor 0x00BBGGRR), `pen_width Int` (always 1), `points Obj` (TSerializeList of TFloatPoint, absolute px)
- **TPolyline**, **TGraphicBox**, **TMessageBox**: ⊃ TSegment / TPolygon, no own fields
- **TPolygon** (0x454AD8): ⊃ TSegment, `brush_color Int` (−1 = none)
- **TGraphicText** (0x456B00): ⊃ TPolygon (`points` = 4-corner bounding box), then:
  - `font_size Int` (pt, 6..26)
  - `font_color Int`
  - `flag_8d Bool` (unknown; probably italic)
  - `font_weight Int` (400 normal / 700 bold)
  - `angle F32` (degrees: 0/90/180/270/360)
  - `text Str`
  - `flag_94 Bool` (unknown)
  - `has_overbar Bool`, then if true `overbar Obj` (a TPolyline)

### Pins, wires, junctions

- **TPin** (0x455A44): ⊃ TPolygon (`points` = 8×8 px square outline), `pin_kind Int` (0 = tag/unassigned, 1 = input, 2 = output, 3 = tri-state output or bidirectional), `wire Obj` (TWire or nil). A pin has at most one wire. Fan-out goes through solders.
- **TMacroPin** (0x455BB8): ⊃ TPin, `partner Obj` (the paired TMacroPin, §5). In 8 corpus objects (74hc20/74hc21) this reference points at a TFloatPoint, which is a stale pointer. **Type-check and treat it as nil.**
- **TWire** (0x455D04): ⊃ TSegment (TPolyline; `points` = polyline, first point = end1, last point = end2), `unk_int_3c Int` (always 0), `end1 Obj`, `end2 Obj` (TPin / TMacroPin / TSolder; nil allowed). `pen_color` is 0 or 0xC0C0C0 (meaning unknown, cosmetic).
- **TSolder** (0x45666C): **does not call TPolygon.Read**. `x F32, y F32, wire1 Obj, wire2 Obj, wire3 Obj`. In the corpus all 3 are always set: a solder is a 3-way junction.

### Devices

- **TDevice** (0x457600):
  ```
  has_graphics Bool
  if has_graphics: children Obj          ; TGraphicList.Read: own drawing (TSerializeList of TPolygon/TPolyline/TGraphicText)
  interactive Bool                       ; True only for TInteractiveInput/TInteractiveSwitch
  flag_1c Bool                           ; unknown (always True for TTagDevice; macros mixed; probably "display pins")
  pins Obj                               ; TGraphicList whose children = TSerializeList of TPin/TMacroPin, in pin order
  if !has_graphics: x F32, y F32         ; device centre (px)
  ```
  Gates, tri-states, flip-flops, TLEDDevice, TSupply, TGround, TClockDevice and TBitGenerator have **no Read override**. Everything about them is in TDevice (+ the parent chain below). The gate input count is in the class name only.
- **TTextDevice** (0x457AE0): ⊃ TDevice, `has_label Bool`; if true `text_pos Int` (0..3), `name_label Obj` (TGraphicText)
- **THistoryDevice** (0x457F74): ⊃ TTextDevice, `logic_history Bool`
- **TInputDevice**, **TSupply**, **TGround**: ⊃ THistoryDevice, nothing more
- **TInteractiveInput** (0x4815EC): ⊃ THistoryDevice, `pulse Bool` (probable), `state Int` (0 = low, 65535 = high)
- **TSeqInputDevice** (0x45EE80) → **TClockDevice**, **TBitGenerator**: ⊃ THistoryDevice, `sequence Str` ('0'/'1' chars; clock = "01"), `color Int` (LED colour: clock 0x00FF00, sequence generator 0x0000FF)
- **TInteractiveSwitch** (0x48127C): ⊃ **TTextDevice** (not THistory), `switch_type Int` (0 = toggle; push-to-make/break presumably 1/2), `flag_3c Bool` (unknown)
- **TOutputDevice** (0x45F6E8) → **TLEDDevice**: ⊃ THistoryDevice, `color Int`
- **TLEDSegmentDevice** (0x45FEE8): ⊃ TOutputDevice, **only if version ∈ {0x1E, 0x22}**: `common_cathode Bool` (True = segments active high; False = active low / common anode; verified by simulating `Counter.dwm`)
- **TNumericDevice** (0x460420): ⊃ TOutputDevice, then:
  - `input_lines Int`
  - `unk_58 Int` (17..26, probably font size)
  - `unk_5c Int` (always 0, probably display format)
  - `unk_color_60 Int` (always 0x808080, probably the dim colour for suppressed leading zeros)
  - `flag_64 Bool`
- **TMemoryDevice** (0x48BDA4): ⊃ TDevice, then:
  - `data_bits Int`
  - `address_bits Int`
  - `is_ram Bool`
  - `unused Bool` (read and discarded)
  - `word_count Int` (= 2^address_bits)
  - `word_count × Int` (contents, stored as u16)
  
  It is always drawn with `has_graphics`.
- **TTagDevice** (0x461F14): ⊃ TDevice (always `has_graphics`; the drawing includes the pin-number text), `text_pos Int` (0..3), `direction Int` (1 in, 2 out, 3 tri/bidir, 0 unassigned), `macro_pin Obj` (inner TMacroPin). Its single TPin (kind 0) is the inner connection point.
- **TMacro** (0x46264C): ⊃ TDevice (always `has_graphics`: the icon/template drawing; `pins` = the external TMacroPins in **DIL order**, 1-based index = pin number), then:
  ```
  datasheet  Str     ; relative PDF path or ''
  unk_int_10 Int     ; always 0 (converted with IntToStr into a string field)
  embedded_38 Bool   ; False for the root macro, True for embedded copies
  embedded_3a Bool   ; same distribution as embedded_38
  devices Obj        ; TGraphicList: gates, FFs, tri-states, memories, switches, nested TMacro, TGraphicText annotations
  tags    Obj        ; TGraphicList: TTagDevice
  inputs  Obj        ; TGraphicList: TInteractiveInput, TClockDevice, TBitGenerator, TSupply, TGround
  outputs Obj        ; TGraphicList: TLEDDevice, TLEDSegmentDevice, TNumericDevice
  solders Obj        ; TGraphicList: TSolder
  wires   Obj        ; TGraphicList: TWire
  ```
  The order above is the exact read order (object fields +0x3C, +0x40, +0x4C, +0x50, +0x48, +0x44).

### Classes that exist in the EXE

Every `TSerialize` descendant is listed in `PARENT` in `dwm_parse.py`. There are no RAM/ROM-specific classes (memory = `TMemoryDevice`), no 5–8-input gates, and no 3/4-input XOR/XNOR.

## 5. Connectivity graph

Within one macro level:

1. Each **wire** connects `end1` and `end2` (a pin, a macro pin or a solder).
2. Each **solder** connects its 3 wires.
3. A **net** is the connected component of {pins, wires, solders} under those relations (union-find).

**Device pins.** The `pins` list order is fixed per class (pin offsets relative to x,y for unrotated devices):

| class | pin order |
|---|---|
| gates | `[OUT(+30,0), IN…(−30, bottom→top)]` |
| NOT | `[OUT(+30,0), IN(−20,0)]` |
| TTriState | `[IN(−20,0), OUT(+20,0) kind 3, EN(0,+10)]` |
| TDFlipFlop | `[Q(+30,−10), /Q(+30,+10), D(−30,−10), CLK(−30,0), CLR(−20,+20), PRE(−10,+20)]` |
| TJKFlipFlop | as D, with `J` in place of D, and `K(−30,+10)` appended |
| TRSFlipFlop | `[Q, /Q, S?(−30,−10), R?(−30,+10)]` (S/R not verified) |
| TLEDSegmentDevice | `[a…g]` at (−15..+15, +50) |
| TMemoryDevice | `[A(n−1)…A0, D(m−1)…D0, CS, R/W(RAM only)]` |
| sources (input, clock, seq, supply, ground) | `[OUT]` |
| TLEDDevice | `[IN]` |
| TInteractiveSwitch | `[T1, T2]` (both kind 3) |

The FF order (including CLR/PRE) was verified against the datasheet pin numbers of the 74HC74 and 74HC76 macros, and the tri-state order against 74HC125. The gate order comes from geometry (gate inputs are commutative).

**Macro boundary.** A nested TMacro instance appears in the parent's `devices`. Its `pins` are TMacroPins in the **parent's** coordinates, and parent wires attach to them. Inside the macro:

```
outer TMacroPin P (macro.pins[k])  --partner-->  inner TMacroPin Q  <--macro_pin--  TTagDevice T
Q.partner == P,  T.pins[0] (kind 0) is wired to the inner net.
```

So `net(P in parent) ≡ net(T.pins[0] in macro)`. DIL pin number = k+1 = the tag's displayed text. Outer pins whose `partner` is nil (e.g. Vcc/GND of an IC) are unconnected inside.

**Embedded copies.** A nested macro is a full copy of the macro file's root object. There are no external file references; Parts-Centre file names are not stored. The root macro's own `pins`/`children` are its template, as seen when the file is inserted elsewhere.

## 6. Coordinates and geometry

- Units are **screen pixels** as float32 (almost always integral). The y axis points down.
- The default grid is 5 px. 99.3 % of device x,y and 99 % of solder x,y are multiples of 5. Pin outlines are 8×8 squares centred on the connection point, so their corners are at ±4.
- **Wire endpoints coincide exactly with the pin-outline centre** (111 696 of 112 000 ends; the rest are off by ≤0.5 px) or with the solder x,y.
- 99.2 % of wire segments are axis-parallel. A few diagonal segments exist (1 186 of 141 627).
- **Rotation is not stored as an angle.** An unrotated device stores `x,y` (centre) and DW redraws the standard symbol. A rotated or custom device has `has_graphics = True`: its drawing and pin outlines are stored pre-rotated in absolute coordinates, and **no x,y** is stored. The importer must derive the centre (bbox of pins+graphics) and the orientation (direction from the input-pin centroid to the output pin). Text rotation is explicit (`TGraphicText.angle`).
- The root macro's `pins` and `children` are in template coordinates (centred near 0,0). Nested macro internals keep their own coordinates, independent of where the instance is placed. The importer should treat each macro level as its own canvas.

## 7. `.dwt` templates

Magic `0x0014ED19` (`04 19 ed 14 00`), no header. The body is a sequence of top-level objects sharing one class table and one object table:

1. `TSerializeList` of graphics (TPolygon / TPolyline / TGraphicText).
2. `TGraphicList` whose children are TMacroPins (kind 0, partner nil, wire nil): the DIL pin outlines in order.

Read objects until EOF; the corpus has exactly 2 roots per file. Files: 8/14/16/18/20/24/28 Pin DIL and Relay.

## 8. `.map` memory maps

Plain binary, written with Pascal file I/O (loader at 0x495098):

```
u32 LE magic = 0x0000ABCD
u32 LE count
count × u32 LE word      ; only the low 16 bits are meaningful
```

Loading truncates to the device size (`count = min(count, words)`). For example, `low1.map` has count 512 and is 2056 bytes. Memory contents are **also embedded** in the `.dwm` (TMemoryDevice), so `.map` files are only import/export conveniences.

## 9. Robustness notes for an importer

- Fields are positional. **An unknown class name is fatal**: its fields cannot be skipped, because there are no lengths. Map class name → reader through the parent chain (e.g. `T3OrGate` → `TDevice` reader).
- Validate every `Obj` result against the expected class family. Corrupt pointers exist in real files (TMacroPin.partner → TFloatPoint).
- Limits used by the reference parser:
  - 64 MiB file
  - 2 M objects (largest real file: 408 k objects / 5.1 MB, `Micro 5.dwm`)
  - 200 k nesting (max seen: 88)
  - 1 MiB strings
  - 1 M list items
  - 1 M memory words (max seen: 16 384)
- Macro nesting depth seen: ≤ 6.
- Reject non-finite floats and trailing bytes (none in the corpus).
- The reader must be **iterative** (explicit stack). A hostile file can nest objects arbitrarily deep.

## 10. Unknowns

| field | hex / values seen | guess |
|---|---|---|
| v30 header `Bool, Str, Str, Int` (1st, 3rd, 4th) | `08`, `06 00`, `02 00` in all 51 files | legacy flags |
| v34 props 102 / 200 / 202 | `02 01 02 66 06 01 30`, `02 02 03 c8 00 02 01`, `02 02 03 ca 00 02 00` | display options (show pins? compact?) |
| TDevice.flag_1c | `09` for all tags, 2076/3257 macros | "display template pins" |
| TGraphicText.flag_8d / flag_94 | 739 True / 47 417 True | italic / ? |
| TWire.unk_int_3c | always `02 00` | — |
| TWire.pen_color | 0 or 0xC0C0C0 | cosmetic |
| TMacro.unk_int_10 | always `02 00` | version/ID string |
| TMacro.embedded_38 / _3a | False only for root | "is embedded" / "locked" |
| TInteractiveInput.pulse | always False | Pulse Input |
| TInteractiveSwitch.switch_type / flag_3c | only 0 / False seen | type / state |
| TNumericDevice.unk_58 / unk_5c / unk_color_60 / flag_64 | 17..26 / 0 / 0x808080 / mostly False | font size / format / dim colour / two's complement or pins-at-bottom |
| TMemoryDevice 2nd Bool | always False, discarded by the reader | legacy ("Compact on save"?) |
| TRSFlipFlop S/R order | — | S = top (−30,−10) |

## 11. Notes from the bitwerk importer

Findings made while writing the Rust importer (`crates/bw-io/src/dwm/`),
checked against every file of the package:

- **v30 header.** The second string is the datasheet path, like property 101
  of version 34 (13 of the 51 version-30 files have one).
- **v34 property 201.** Seen in 17 files: an 8–15 byte binary string of
  unknown meaning (a checksum or registration key?). Ignored and reported.
- **Numeric output geometry.** In the samples the input pins run along the
  *top* of the display (pin 0 = least significant bit on the right) and the
  digits are drawn below them. The display format is not reliably in
  `unk_5c` (always 0); the number of digit texts in the stored drawing
  identifies it (8 digits for 8 lines = binary, 2 = hexadecimal, otherwise
  decimal). `flag_64` is set on two decimal displays that show signed
  values: the importer reads it as "two's complement".
- **7-segment pins** are 5 px (one grid unit) apart along the bottom.
- **Interactive switch** (`TInteractiveSwitch`): a two-terminal contact,
  used vertically (terminals 30 px apart) in Divider, RTX and assignment.
- **Macro sizes.** Embedded macros reach 154 inputs (an interrupt
  controller), 128 outputs (a 7 → 128 decoder) and, once, 361 pins (the
  control unit of `Micro 5.dwm`).
- **Content dedup.** `dedup.py` and the Rust hash agree per file (for
  example Bus Contention: 9 copies, 4 implementations; Counter: 7 copies,
  7 implementations).
- **Initial state.** Digital Works evaluates with two-valued signals from 0
  and sequential (immediate) propagation; gate-level latches such as the
  master-slave JK sample only work under those semantics. bitwerk's
  "Sequential" evaluation mode reproduces them (see the README).
