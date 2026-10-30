//! The round screen's **chrome**, and the three widgets it draws every frame — from the player's own
//! files.
//!
//! Stage 27c established the shape of this screen (`recon/round/port_check/STAGE27C_SPEC.md`):
//!
//! * **There is no chrome-drawing code.** `CS:0x29F3` sets video mode 4 and reads the 16,192 bytes of
//!   `TRAP.PIC` **straight into `B800:0000`** (`mov cx,0x3f40; rep` — no header, no palette, no
//!   compression). Every static pixel of the round screen — the console casings, the two viewport
//!   frames, the `WHITE SPY`/`BLACK SPY`/`BODY HEAT` captions, the gauge wells, the clock pod, the
//!   kit-box wells, the tray strip — is a byte of that file, copied and not drawn.
//! * The only pixels the game writes **outside** the viewports are four widgets, each redrawn on its
//!   own schedule: the body-heat gauge (`CS:0x21BD`), the clock's four digits (`CS:0x229E`), the
//!   seven tray dots (`CS:0x3278`, and the blinking slot `CS:0x2CDA`) and the kit box (`CS:0x31C8`).
//!
//! So `assets/round_bg.bin` (the plate this port used to start each frame from) is `TRAP.PIC` plus
//! 802 pixels that belong to those widgets, and `assets/round_kitbox.bin` / `assets/round_digits.bin`
//! are decodes of records that ship in `spy.exe`. This module computes all three, using the
//! original's own code paths:
//!
//! ```text
//! plate  = decode(TRAP.PIC) with both viewport rects filled flat cyan (level 0's plate, § 1)
//! gauge  = a 60 x 3 byte bitmap seeded from DS:0x160A and stamped with the two 14-byte row records
//!          DS:0x15EE (cyan) / DS:0x15FC (white) through the masked blitter            CS:0x21BD
//! kit    = a 10 x 11 byte white canvas stamped with the three icon records
//!          DS:0x6E5C / 0x6EA4 / 0x6E82                                              CS:0x31C8
//! dots   = seven opaque 4-row records, held / absent, at the tray columns            CS:0x3278
//! clock  = four opaque 8 x 2 glyph records out of DS:0x7004 + 18*d                    CS:0x229E
//! ```
//!
//! ## The two record formats (they are not the same, and that is what stage 26a got wrong)
//!
//! * **`CS:0x3455`, the masked blitter** — `[h][w]` then an AND plane and an OR plane of `h*w` bytes
//!   each, used as `dst = (dst & and) | or` one destination plane at a time. `AND = 3` keeps the
//!   destination (transparent); a record whose OR plane is all zero (the middle kit icon) is a black
//!   cut-out. It can mirror (`DX != 0`) and clip at the top and at both byte edges. **No scaling**:
//!   398 bytes, and the entry is `[rows][bytes]`, not `[width][height]`.
//! * **`CS:0x45B0`, the record → screen copier** — `[h][w]` then **one** opaque plane, and source row
//!   *r* lands on screen row `bx + r`, one for one. The `0x2000`/`0x1FB0` pair inside it is the CGA
//!   even/odd scanline split, not a vertical stretch — which is why the kit box's 10 rows reach 10
//!   screen rows.
//!
//! Everything here is a *read* of `spy.exe`'s data segment (`DS:0 == file 0x400`) or of `TRAP.PIC`;
//! no captured pixels and no invented geometry. The addresses are the ones the code uses, and a
//! caller can change a byte in the image and see the output change.

// The module is the port of four routines plus the record formats they share, and it documents each
// record's address, size and geometry as a constant even where only a check (or a probe through
// `recon/`) reads it back. `round.rs` does the same for the same reason.
#![allow(dead_code)]

use crate::round::VIEW_BOTTOM;
use crate::round::VIEW_TOP;
use crate::{H, PIXELS, W};

// ---- the player's files -------------------------------------------------
/// `TRAP.PIC` — the round screen, read straight into `B800:0000` by `CS:0x29F3`.
/// 16,192 = 0x3F40 bytes: 8,192 (interlace field 0) + 8,000 (field 1, 100 rows x 80 bytes). The
/// last 192 bytes of the 16,384-byte CGA image are simply not read, so the file has no header.
pub const TRAP_PIC_BYTES: usize = 0x3F40;
/// The name the loader opens (`DS:0x0004`, lower case in the string table).
pub const TRAP_PIC_NAME: &str = "TRAP.PIC";

/// `DS:0 == file 0x400` (segment 0x1020).
pub const DS_FILE_OFF: usize = 0x400;

// ---- data-segment addresses (the code's own, never assumed) --------------
/// `DS:0x1494` — the kit box's canvas template: `[10][11]` followed by 110 bytes of `FF`.
pub const DS_KIT_TEMPLATE: usize = 0x1494;
/// `DS:0x248C + bp` / `0x2492 + bp` / `0x2498 + bp` (bp = 0, 2, 4 -> right, middle, left): the kit
/// cell's record pointer, its bottom row (8, 9, 9) and its byte column (8, 4, 0).
pub const DS_KIT_CELL_PTR: usize = 0x248C;
pub const DS_KIT_CELL_BOTTOM: usize = 0x2492;
pub const DS_KIT_CELL_X: usize = 0x2498;
/// `DS:0x249E + si` (si = 0, 2): the box's screen row per console, 76 / 184. The byte column is the
/// code's own `mov $0xa` — 10 bytes, i.e. x 40.
pub const DS_KIT_ROW: usize = 0x249E;
/// `DS:0x2F2C + si` — the carried item code (`0xFFFF` = empty hand).
pub const DS_CARRIED: usize = 0x2F2C;
/// `DS:0x2F4C + si` — the kit box's blink phase (negated once per drawn call).
pub const DS_KIT_BLINK: usize = 0x2F4C;
/// `DS:0x32A3 + si` — the kit word.
pub const DS_KIT: usize = 0x32A3;
/// `DS:0x2D60 + 2*code` — item code -> its bit in the kit word (`0x40`, `0x80`, `0x200` for the
/// three parts). The kit code indexes it with `bp` = 0x16/0x14/0x12 = `2*code`.
pub const DS_CODE_BIT: usize = 0x2D60;
/// `DS:0x322F + bp` (bp = 4, 2, 0 -> codes 11, 10, 9): who has **deposited** that part, in `si`
/// units (0 = white, 2 = black, `0xFFFF` = nobody).
pub const DS_PART_DEPOSIT: usize = 0x322F;
/// `DS:0x2F58 + si` — body heat, counted as cold: 3 is full, 62 is empty, `0x3D` freezes.
pub const DS_HEAT: usize = 0x2F58;
/// `DS:0x15EE` / `DS:0x15FC` — the gauge's two 14-byte row records, `[2][3]` + AND + OR: cyan and
/// white.
pub const DS_HEAT_ROW_CYAN: usize = 0x15EE;
pub const DS_HEAT_ROW_WHITE: usize = 0x15FC;
/// `DS:0x160A` — the gauge's template: `[60][3]` + 180 bytes, the full bar's own image.
pub const DS_GAUGE_TEMPLATE: usize = 0x160A;
/// `DS:0x2F5C + si` — the gauge's screen row per console, 8 / 116. The byte column is the code's
/// `mov $0xe`, i.e. x 56.
pub const DS_GAUGE_ROW: usize = 0x2F5C;
/// `DS:0x15BC + bp` / `DS:0x15CA + bp` (bp = 12, 10, .. 0): the tray dot's record, held / absent.
pub const DS_DOT_HELD: usize = 0x15BC;
pub const DS_DOT_ABSENT: usize = 0x15CA;
/// `DS:0x2E0A + si` — the dot strip's screen row per console, 74 / 182.
pub const DS_DOT_ROW: usize = 0x2E0A;
/// `DS:0x2E0E + bp` — the dot's byte column: 58, 53, 47, 42, 36, 31, 25 (right to left).
pub const DS_DOT_COL: usize = 0x2E0E;
/// `DS:0x2D8C + bp` — trapulator slot -> its kit bit: 1, 2, 4, 8, 0x10, 0x20, 0x800.
pub const DS_SLOT_BIT: usize = 0x2D8C;
/// `DS:0x324F` — ten words: digit -> its glyph record. The records are `[8][2]` + 16 opaque bytes,
/// 18 bytes apart (`DS:0x7004 + 18*d`), i.e. **eight** rows.
pub const DS_DIGIT_PTR: usize = 0x324F;

// ---- geometry (the code's own, all measured) ----------------------------
/// The gauge: 3 bytes x 60 rows at byte column 0x0E (`CS:0x21BD`), i.e. 12 px wide — **not** the 9
/// this port used to copy out of the plate. Bytes 0 and 2 put magenta at x 56/57 and black at x 67
/// and are identical in both row records, which is why x 58..66 is the only part that moves.
pub const GAUGE_COL: usize = 0x0E;
pub const GAUGE_X: usize = GAUGE_COL * 4;
pub const GAUGE_W: usize = 12;
pub const GAUGE_ROWS: usize = 60;
pub const GAUGE_BYTES: usize = 3;
pub const GAUGE_ROW: [usize; 2] = [8, 116];
/// The kit box: 11 bytes x 10 rows at byte column 0x0A, i.e. 44 px wide.
pub const KIT_COL: usize = 0x0A;
pub const KIT_X: usize = KIT_COL * 4;
pub const KIT_W: usize = 44;
pub const KIT_ROWS: usize = 10;
pub const KIT_BYTES: usize = 11;
pub const KIT_ROW: [usize; 2] = [76, 184];
/// The tray dots: seven opaque records of 4 rows at the columns in `DS:0x2E0E`.
pub const DOT_ROWS: usize = 4;
/// The clock: four cells of 8 rows x 2 bytes at row 0x62. The original's four writes, in its own
/// order, are
///
/// ```text
/// 229e  dec word[0x324D] ; v = word[0x324D]
///       digit(v mod 10)        at byte column 0x4B   ; px 300   -- second units
///       digit((v/10) mod 6)    at byte column 0x49   ; px 292   -- second tens
///       digit((v/60) mod 10)   at byte column 0x45   ; px 276   -- minute units
///       digit((v/600) mod 10)  at byte column 0x43   ; px 268   -- minute tens
/// ```
///
/// so the columns ascend with the digit's place: **`CLOCK_COLS[k]` is [`clock_digits`]`[k]`'s
/// column**, minute tens at px 268 through second units at px 300. (Getting this backwards draws a
/// mirror-image clock that every local test still agrees with — it took `frame_check3.py`, which
/// compares against a recorded frame, to see it: 232 px of surround residual on `playtest1`.)
pub const CLOCK_ROW: usize = 98;
pub const CLOCK_COLS: [usize; 4] = [67, 69, 73, 75];
pub const CLOCK_ROWS: usize = 8;

/// The rectangles the four widgets occupy in the 320x200 frame, per console where they differ:
/// `(x0, y0, x1, y1)` with the ends exclusive. Everything outside the two viewports and outside
/// these is `TRAP.PIC`, byte for byte (414 recorded frames, measured: STAGE27C_SPEC § 2).
///
/// The dot strip's rectangle also covers `CS:0x2CDA`'s blinking slot, which stamps one of the same
/// cells in the same rows.
pub const WIDGET_RECTS: [(usize, usize, usize, usize); 7] = [
    (GAUGE_X, GAUGE_ROW[0], GAUGE_X + GAUGE_W, GAUGE_ROW[0] + GAUGE_ROWS),
    (GAUGE_X, GAUGE_ROW[1], GAUGE_X + GAUGE_W, GAUGE_ROW[1] + GAUGE_ROWS),
    (KIT_X, KIT_ROW[0], KIT_X + KIT_W, KIT_ROW[0] + KIT_ROWS),
    (KIT_X, KIT_ROW[1], KIT_X + KIT_W, KIT_ROW[1] + KIT_ROWS),
    // seven 2-byte cells from byte column 25 (x 100) to 59 (x 236); the rectangle is the strip's
    (100, 74, 240, 74 + DOT_ROWS),
    (100, 182, 240, 182 + DOT_ROWS),
    (266, CLOCK_ROW, 310, CLOCK_ROW + CLOCK_ROWS),
];

fn word(ds: &[u8], addr: usize) -> u16 {
    let a = addr & 0xFFFF;
    u16::from_le_bytes([ds[a], ds[(a + 1) & 0xFFFF]])
}

fn sword(ds: &[u8], addr: usize) -> i16 {
    let w = word(ds, addr);
    w as i16
}

// ---- the plate ----------------------------------------------------------
/// The screen's own byte pitch: 320 px / 4 px a byte, the 80 bytes of a CGA row. Every widget here
/// addresses the screen the way the original does — **byte** columns and rows — so the drawing
/// functions all take a byte-indexed 2bpp image with this pitch (`crate::Screen` is one byte per
/// pixel; [`pack`] and [`unpack`] convert).
pub const SCREEN_PITCH: usize = W / 4;

/// Pixels -> the screen's own 2bpp bytes (320 x 200 px -> 80 x 200 bytes), four pixels a byte,
/// leftmost in the high bits.
pub fn pack(px: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; SCREEN_PITCH * H];
    for y in 0..H {
        for j in 0..SCREEN_PITCH {
            let i = y * W + j * 4;
            if i + 3 < px.len() {
                out[y * SCREEN_PITCH + j] =
                    (px[i] << 6) | (px[i + 1] << 4) | (px[i + 2] << 2) | px[i + 3];
            }
        }
    }
    out
}

/// The screen's 2bpp bytes -> one palette index per pixel. The exact inverse of [`pack`].
pub fn unpack(bytes: &[u8]) -> Vec<u8> {
    let mut px = vec![0u8; PIXELS];
    for y in 0..H {
        for j in 0..SCREEN_PITCH {
            let b = bytes[y * SCREEN_PITCH + j];
            px[y * W + j * 4] = (b >> 6) & 3;
            px[y * W + j * 4 + 1] = (b >> 4) & 3;
            px[y * W + j * 4 + 2] = (b >> 2) & 3;
            px[y * W + j * 4 + 3] = b & 3;
        }
    }
    px
}

/// `decode(TRAP.PIC)` — the screen the loader reads into `B800:0000`, as 320x200 palette indices.
///
/// The CGA layout is the interlace the port already uses everywhere: byte
/// `(y & 1) * 0x2000 + (y >> 1) * 80 + x / 4` holds four pixels, leftmost in the high bits.
pub fn decode_trap_pic(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < TRAP_PIC_BYTES {
        return None;
    }
    let mut idx = vec![0u8; PIXELS];
    for y in 0..H {
        let base = (y & 1) * 0x2000 + (y >> 1) * 80;
        let row = y * W;
        for x in 0..W {
            idx[row + x] = (raw[base + x / 4] >> (6 - 2 * (x & 3))) & 3;
        }
    }
    Some(idx)
}

/// **The round plate**: `decode(TRAP.PIC)` with both viewport rects filled flat cyan.
///
/// The fill is not invented: `TRAP.PIC`'s own viewport areas are flat **white** (§ 1 of the spec —
/// measured, 10,208 px each) because the game always paints the room over them, and a console with
/// no room is blanked with plate level 0, which is `0x55` bytes, i.e. flat cyan (`CS:0x3445` ->
/// `CS:0x33CB`). Filling here means a frame composed without its strips still looks like the
/// original's blank console rather than showing TRAP.PIC's placeholder.
///
/// The plate is the file's own bytes, so where a blanked console's widgets are **not** drawn the
/// file shows through — 16 px in `TRAP.PIC`'s own bottom tray (`x 103..104` and `x 148..149`, rows
/// `183..184`) are two held dots that the modal capture `assets/round_bg.bin` had as black. A
/// two-player round draws that tray every frame and never sees the difference; a one-player round
/// leaves the second console blank, and there the port now shows the file rather than the capture.
/// (The original's round-entry draw `CS:0x46F` stamps both trays with `kit = 0x0FFF`; the port does
/// not model that entry draw, which is round-entry state, not chrome.)
pub fn plate(raw: &[u8]) -> Option<Vec<u8>> {
    let mut idx = decode_trap_pic(raw)?;
    for (vx, vy) in [VIEW_TOP, VIEW_BOTTOM] {
        for y in vy..vy + crate::round::STRIP_ROWS {
            for x in vx..vx + 176 {
                idx[y * W + x] = 1;
            }
        }
    }
    Some(idx)
}

// ---- the two record formats --------------------------------------------
/// A **masked** record — `[h][w]` then an AND plane and an OR plane of `h*w` bytes each, the format
/// `CS:0x3455` consumes (`DS:0x15EE`, `0x15FC`, `0x6E5C`, `0x6EA4`, `0x6E82`, the object table, the
/// rocket). `dst = (dst & and) | or`, one destination plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskedRecord {
    /// Rows.
    pub rows: usize,
    /// Bytes per row (4 px each).
    pub bytes: usize,
    pub and: Vec<u8>,
    pub or: Vec<u8>,
}

impl MaskedRecord {
    pub fn at(ds: &[u8], addr: usize) -> Option<MaskedRecord> {
        let a = addr & 0xFFFF;
        let rows = *ds.get(a)? as usize;
        let bytes = *ds.get(a + 1)? as usize;
        if rows == 0 || bytes == 0 {
            return None;
        }
        let n = rows * bytes;
        let and = ds.get(a + 2..a + 2 + n)?.to_vec();
        let or = ds.get(a + 2 + n..a + 2 + 2 * n)?.to_vec();
        Some(MaskedRecord { rows, bytes, and, or })
    }

    /// Stamp into a 2bpp image of `pitch` bytes a row, the record's **bottom** row on `bottom` and
    /// its byte column at `x` — `CS:0x3455`'s arithmetic, clipping included:
    ///
    /// ```text
    /// top = bottom - rows
    /// if top < 0 { skip = -top; top = 0 }        ; the destination's top edge: rows are dropped
    /// dest = base + pitch*top + x                ; per row: dest += pitch, per byte: dest += 1
    /// ```
    ///
    /// The original also clips at the right edge (`0x2620`, the bytes dropped are skipped in both
    /// planes) and at the left (`CX < 0`), and mirrors when `DX != 0`; every chrome caller passes
    /// `DX = 0` and a destination wide enough, but the clip is kept because a caller that moves a
    /// record to the strip's edge depends on it.
    pub fn stamp(&self, dst: &mut [u8], pitch: usize, x: i32, bottom: i32) {
        self.stamp_mirrored(dst, pitch, x, bottom, false)
    }

    pub fn stamp_mirrored(
        &self,
        dst: &mut [u8],
        pitch: usize,
        x: i32,
        bottom: i32,
        mirror: bool,
    ) {
        let mut top = bottom - self.rows as i32;
        let mut skip = 0i32;
        if top < 0 {
            skip = -top;
            top = 0;
        }
        for r in 0..(self.rows as i32 - skip) {
            let y = top + r;
            if y < 0 {
                continue;
            }
            let row = y as usize * pitch;
            for i in 0..self.bytes as i32 {
                let xb = if mirror { x + (self.bytes as i32 - 1 - i) } else { x + i };
                if xb < 0 || xb >= pitch as i32 {
                    continue;
                }
                let d = row + xb as usize;
                if d >= dst.len() {
                    continue;
                }
                let s = (skip + r) as usize * self.bytes + i as usize;
                let (mut a, mut o) = (self.and[s], self.or[s]);
                if mirror {
                    // `CS:0x34..`: the sixteen-instruction ror/shr chain reverses each source
                    // byte's four 2-bit pixels and keeps their values (0x1B -> 0xE4). It is not a
                    // bit reversal: colours 1 and 2 do not swap.
                    a = crate::round::reverse_pixels(a);
                    o = crate::round::reverse_pixels(o);
                }
                dst[d] = (dst[d] & a) | o;
            }
        }
    }
}

/// An **opaque** record — `[h][w]` then **one** plane of `h*w` bytes, the format `CS:0x45B0`
/// consumes (`DS:0x7004`, `0x15BC`/`0x15CA`, and the gauge bitmap itself). Source row *r* lands on
/// destination row `row + r`, one for one; the `0x2000`/`0x1FB0` pair in the original is the CGA
/// interlace, not a stretch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpaqueRecord {
    pub rows: usize,
    pub bytes: usize,
    pub plane: Vec<u8>,
}

impl OpaqueRecord {
    pub fn at(ds: &[u8], addr: usize) -> Option<OpaqueRecord> {
        let a = addr & 0xFFFF;
        let rows = *ds.get(a)? as usize;
        let bytes = *ds.get(a + 1)? as usize;
        if rows == 0 || bytes == 0 {
            return None;
        }
        let n = rows * bytes;
        Some(OpaqueRecord { rows, bytes, plane: ds.get(a + 2..a + 2 + n)?.to_vec() })
    }

    /// Copy onto a 2bpp image of `pitch` bytes a row, first row on `row`, byte column `col`.
    pub fn copy_to(&self, dst: &mut [u8], pitch: usize, row: i32, col: i32) {
        for r in 0..self.rows as i32 {
            let y = row + r;
            if y < 0 {
                continue;
            }
            for i in 0..self.bytes as i32 {
                let x = col + i;
                if x < 0 || x >= pitch as i32 {
                    continue;
                }
                let d = y as usize * pitch + x as usize;
                if d < dst.len() {
                    dst[d] = self.plane[r as usize * self.bytes + i as usize];
                }
            }
        }
    }
}

/// One 2bpp byte -> its four pixel values, leftmost first.
fn pixels(b: u8) -> [u8; 4] {
    [(b >> 6) & 3, (b >> 4) & 3, (b >> 2) & 3, b & 3]
}

// ---- the body-heat gauge (CS:0x21BD) -----------------------------------
/// **The gauge's bitmap for a heat level**, 60 rows x 3 bytes, as the routine leaves it.
///
/// The original keeps one bitmap per player (`DS:0x330B` -> `DS:0x16C0` / `DS:0x177C`), seeds it
/// from the template `DS:0x160A` and then, per heat step, stamps **two rows** of the cyan or the
/// white row record into it and copies the whole thing to the screen. Read off `CS:0x21BD`:
///
/// ```text
/// warming (ax >= 0): if level <= 3 -> nothing at all
///                    level -= 2;  bx = level;  record = 0x15EE (cyan)
/// cooling (ax <  0): bx = level;  if level <= 0x3D level += 2;  record = 0x15FC (white)
/// call 0x3455 (stride 3, DX 0, BX = bx, CX 0, DI = bitmap)   ; rows bx-2, bx-1
/// ```
///
/// The stamp's bottom row is always **odd** (the level starts at 3 and moves by 2), so the record's
/// row 0 lands on the odd bitmap rows and its row 1 on the even ones. Settled, that is a pure
/// function of the level `L`:
///
/// ```text
/// row 0        = the template's own row (`a8 00 02`: the cap, never stamped)
/// rows 1..L-3  = the WHITE row, by parity        (drained)
/// rows L-2..59 = the CYAN row, by parity         (heat remaining)
/// ```
///
/// `None` if the template or either row record does not parse.
pub fn heat_bitmap(ds: &[u8], level: i16) -> Option<Vec<u8>> {
    let cyan = MaskedRecord::at(ds, DS_HEAT_ROW_CYAN)?;
    let white = MaskedRecord::at(ds, DS_HEAT_ROW_WHITE)?;
    if cyan.rows != 2 || white.rows != 2 || cyan.bytes != GAUGE_BYTES || white.bytes != GAUGE_BYTES {
        return None;
    }
    // the template: `[60][3]` then the plane
    let tmpl = OpaqueRecord::at(ds, DS_GAUGE_TEMPLATE)?;
    if tmpl.rows != GAUGE_ROWS || tmpl.bytes != GAUGE_BYTES {
        return None;
    }
    let mut bmp = tmpl.plane;
    // the level the code would have: 3 at the round start, 62 when frozen out. `L - 3` rows are
    // drained, which is 59 at the bottom of the scale — the whole gauge below its cap.
    let l = level.clamp(3, 62) as usize;
    let white_rows = l.saturating_sub(3).min(GAUGE_ROWS - 1);
    let stamp_row = |bmp: &mut Vec<u8>, row: usize, rec: &MaskedRecord| {
        let src = if row % 2 == 1 { 0 } else { 1 };
        for i in 0..GAUGE_BYTES {
            let a = rec.and[src * GAUGE_BYTES + i];
            let o = rec.or[src * GAUGE_BYTES + i];
            let d = row * GAUGE_BYTES + i;
            bmp[d] = (bmp[d] & a) | o;
        }
    };
    for row in 1..=white_rows {
        stamp_row(&mut bmp, row, &white);
    }
    for row in white_rows + 1..GAUGE_ROWS {
        stamp_row(&mut bmp, row, &cyan);
    }
    Some(bmp)
}

/// **`CS:0x21BD`'s screen half**: the 60 x 3 bitmap at byte column 0x0E of the console's gauge row
/// (`DS:0x2F5C + 2*player`), i.e. 12 px at x 56, rows 8..67 / 116..175.
///
/// `scr` is the screen's **byte** image ([`SCREEN_PITCH`]); everything here is byte-addressed
/// exactly as the original's `di = row_table[bx] + cx` is.
pub fn draw_heat_gauge(scr: &mut [u8], ds: &[u8], player: usize, level: i16) -> bool {
    let Some(bmp) = heat_bitmap(ds, level) else { return false };
    let row = word(ds, DS_GAUGE_ROW + 2 * player) as usize;
    let rec = OpaqueRecord {
        rows: GAUGE_ROWS,
        bytes: GAUGE_BYTES,
        plane: bmp,
    };
    rec.copy_to(scr, SCREEN_PITCH, row as i32, GAUGE_COL as i32);
    true
}

// ---- the kit box (CS:0x31C8) -------------------------------------------
/// The kit box's inputs, as the code reads them: the kit word, the carried code, the blink phase
/// and the four-word "who deposited this part" ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KitState {
    /// `DS:0x32A3 + 2*player`.
    pub kit: u16,
    /// `DS:0x2F2C + 2*player` — `0xFFFF` = nothing in hand.
    pub carried: u16,
    /// `DS:0x2F4C + 2*player` — only the sign matters: the whole carried set blinks.
    pub blink: i16,
    /// `DS:0x322D` — the five-word part-owner ledger (codes 8..12).
    pub owner: [u16; 5],
}

impl KitState {
    pub fn from_ds(ds: &[u8], player: usize) -> KitState {
        let mut owner = [0u16; 5];
        for (k, o) in owner.iter_mut().enumerate() {
            *o = word(ds, DS_PART_DEPOSIT - 2 + 2 * k);
        }
        KitState {
            kit: word(ds, DS_KIT + 2 * player),
            carried: word(ds, DS_CARRIED + 2 * player),
            blink: sword(ds, DS_KIT_BLINK + 2 * player),
            owner,
        }
    }
}

/// The three cells in the order the code draws them: `bp` = 0x16, 0x14, 0x12 -> **left, middle,
/// right** = codes 11, 10, 9. `(record address, x in bytes, bottom row)` from the three tables.
pub fn kit_cells(ds: &[u8]) -> [(u16, i32, i32); 3] {
    let mut out = [(0u16, 0i32, 0i32); 3];
    for (n, bp) in [4usize, 2, 0].iter().enumerate() {
        out[n] = (
            word(ds, DS_KIT_CELL_PTR + bp),
            word(ds, DS_KIT_CELL_X + bp) as i32,
            word(ds, DS_KIT_CELL_BOTTOM + bp) as i32,
        );
    }
    out
}

/// **The kit box's canvas** (10 rows x 11 bytes) for one console: the template, then the two passes.
///
/// ```text
/// ; pass 1 — the carried set, only while a part or the case is in hand and the blink is positive
/// if carried >= 9 and blink >= 0:
///     for bp = 0x16, 0x14, 0x12:  if kit & word[0x2D60+bp]:  stamp(record(bp-0x12), x, bottom)
/// ; pass 2 — the deposited set, never gated on carriage or blink
/// for bp = 4, 2, 0:  if word[0x322F+bp] == si and (kit & word[0x2D60+bp+0x12]) == 0:
///     stamp(record(bp), x(bp), bottom(bp))
/// ; then blit the canvas to the screen and negate the blink
/// ```
///
/// So a cell is stamped solid when *this* spy has deposited that part even with its kit bit clear
/// — the owner's "a found rocket part does not light in the kit display" — and blinks when it is
/// in the case (`carried >= 9`, kit bit set).
pub fn kit_canvas(ds: &[u8], player: usize, st: &KitState) -> Option<Vec<u8>> {
    // the template: 110 payload bytes of `FF` (`DS:0x1494` is `[10][11]` + the plane)
    let mut canvas = ds.get(DS_KIT_TEMPLATE + 2..DS_KIT_TEMPLATE + 2 + KIT_ROWS * KIT_BYTES)?
        .to_vec();
    if canvas.len() != KIT_ROWS * KIT_BYTES {
        return None;
    }
    let cells = kit_cells(ds);
    let si = 2 * player as u16;
    let stamp = |canvas: &mut Vec<u8>, cell: usize| {
        let (addr, x, bottom) = cells[cell];
        if let Some(rec) = MaskedRecord::at(ds, addr as usize) {
            rec.stamp(canvas, KIT_BYTES, x, bottom);
        }
    };
    // pass 1 — `CS:0x31e7`: bp = 0x16, 0x14, 0x12 (codes 11, 10, 9), gated on carriage and blink
    if st.carried != 0xFFFF && st.carried >= 9 && st.blink >= 0 {
        for (cell, bp) in [0x16usize, 0x14, 0x12].iter().enumerate() {
            if st.kit & word(ds, DS_CODE_BIT + bp) != 0 {
                stamp(&mut canvas, cell);
            }
        }
    }
    // pass 2 — `CS:0x321f`: bp = 4, 2, 0 (codes 11, 10, 9), gated on the deposit ledger
    // (`DS:0x322F + bp`, i.e. `st.owner` at the part's `code - 8`)
    for (cell, bp) in [4usize, 2, 0].iter().enumerate() {
        if st.owner[bp / 2 + 1] == si && st.kit & word(ds, DS_CODE_BIT + bp + 0x12) == 0 {
            stamp(&mut canvas, cell);
        }
    }
    Some(canvas)
}

/// **`CS:0x31C8`'s screen half**: the canvas at byte column 0x0A (x 40), rows 76 / 184.
pub fn draw_kit_box(scr: &mut [u8], ds: &[u8], player: usize, st: &KitState) -> bool {
    let Some(canvas) = kit_canvas(ds, player, st) else { return false };
    let row = word(ds, DS_KIT_ROW + 2 * player) as i32;
    let rec = OpaqueRecord { rows: KIT_ROWS, bytes: KIT_BYTES, plane: canvas };
    rec.copy_to(scr, SCREEN_PITCH, row, KIT_COL as i32);
    true
}

/// The three kit icons as this port's `assets/round_kitbox.bin` stores them: for each cell alone on
/// the canvas, the 40 x 10 px box with `255` wherever the white canvas shows through, plus an
/// eleventh all-transparent row (the box is 10 rows; the asset's 11th is filler).
///
/// This is the derivation the asset was captured to stand in for — and it is byte-identical to it
/// (`the_kit_icons_are_computed_from_the_records`).
pub fn kit_icons(ds: &[u8]) -> Option<Vec<[u8; 440]>> {
    let mut out = Vec::with_capacity(3);
    for cell in 0..3usize {
        let mut canvas = ds.get(DS_KIT_TEMPLATE + 2..DS_KIT_TEMPLATE + 2 + KIT_ROWS * KIT_BYTES)?
            .to_vec();
        let (addr, x, bottom) = kit_cells(ds)[cell];
        MaskedRecord::at(ds, addr as usize)?.stamp(&mut canvas, KIT_BYTES, x, bottom);
        let mut icon = [255u8; 440];
        for r in 0..KIT_ROWS {
            for b in 0..10 {
                for (k, v) in pixels(canvas[r * KIT_BYTES + b]).into_iter().enumerate() {
                    icon[r * 40 + b * 4 + k] = if v == 3 { 255 } else { v };
                }
            }
        }
        out.push(icon);
    }
    Some(out)
}

// ---- the tray dots (CS:0x3278) -----------------------------------------
/// **`CS:0x3278`** — the seven tray dots, one opaque 4-row record each, at `DS:0x2E0E`'s byte
/// columns and `DS:0x2E0A`'s row, held or absent by the kit word's seven bits.
///
/// The loop runs `bp` = 0x0C down to 0, so the **map** (bit 0x800, byte column 58, x 232) is drawn
/// first and the detonator (bit 1, column 25, x 100) last; the records encode the whole little box
/// (its outline as well as the 2x2 core), so they are stamped, not merged.
pub fn draw_tray_dots(scr: &mut [u8], ds: &[u8], player: usize, kit: u16) -> usize {
    let row = word(ds, DS_DOT_ROW + 2 * player) as i32;
    let mut drawn = 0;
    for bp in (0..=0x0Cu16).step_by(2).rev() {
        let col = word(ds, DS_DOT_COL + bp as usize) as i32;
        let held = kit & word(ds, DS_SLOT_BIT + bp as usize) != 0;
        let table = if held { DS_DOT_HELD } else { DS_DOT_ABSENT };
        let Some(rec) = OpaqueRecord::at(ds, word(ds, table + bp as usize) as usize) else {
            continue;
        };
        rec.copy_to(scr, SCREEN_PITCH, row, col);
        drawn += 1;
    }
    drawn
}

// ---- the clock (CS:0x229E) ---------------------------------------------
/// The four digit cells' values for `secs`, in cell order (minute tens, minute units, second tens,
/// second units).
///
/// `CS:0x229E` decrements `DS:0x324D` and takes `v % 10`, `(v/10) % 6`, `(v/60) % 10` and
/// `(v/600) % 10`. The middle two are the original's own shortcuts, and they are exact for every
/// value a round reaches: with `v = 60q + s` (0 <= s < 60), `v/10 = 6q + s/10`, so `(v/10) % 6 =
/// s/10 = (v % 60) / 10`; and `(v/600) % 10` is `(v/60)/10` while `v < 6000`, which is 11 minutes
/// past the longest round.
pub fn clock_digits(secs: u32) -> [u8; 4] {
    let v = secs;
    [
        ((v / 600) % 10) as u8,
        ((v / 60) % 10) as u8,
        ((v / 10) % 6) as u8,
        (v % 10) as u8,
    ]
}

/// **`CS:0x229E`'s screen half**: four 8 x 2 opaque glyph records at row 0x62, at the byte columns
/// [`CLOCK_COLS`] holds in [`clock_digits`]' order — `0x43` (px 268) for the minute tens through
/// `0x4B` (px 300) for the second units.
///
/// The records are `[h=8][w=2]` + 16 bytes, 18 bytes apart at `DS:0x7004 + 18*d`, and they are
/// drawn as **whole 8-row blocks**: row 0 is uniformly the pod's own colour, which is why the
/// seven-row glyph asset this port used to carry was display-complete.
pub fn draw_clock(scr: &mut [u8], ds: &[u8], secs: u32) -> bool {
    let digits = clock_digits(secs);
    for (k, d) in digits.iter().enumerate() {
        let addr = word(ds, DS_DIGIT_PTR + 2 * (*d as usize));
        let Some(rec) = OpaqueRecord::at(ds, addr as usize) else { return false };
        if rec.rows != CLOCK_ROWS || rec.bytes != 2 {
            return false;
        }
        rec.copy_to(scr, SCREEN_PITCH, CLOCK_ROW as i32, CLOCK_COLS[k] as i32);
    }
    true
}

/// The ten glyphs as 8 x 8 pixel images (rows 0..7, in record order), for the tests and for a
/// `--check`-style report.
pub fn digit_glyphs(ds: &[u8]) -> Option<Vec<[[u8; 8]; 8]>> {
    let mut out = Vec::with_capacity(10);
    for d in 0..10usize {
        let rec = OpaqueRecord::at(ds, word(ds, DS_DIGIT_PTR + 2 * d) as usize)?;
        if rec.rows != CLOCK_ROWS || rec.bytes != 2 {
            return None;
        }
        let mut g = [[0u8; 8]; 8];
        for r in 0..CLOCK_ROWS {
            let p = pixels(rec.plane[r * 2]);
            let q = pixels(rec.plane[r * 2 + 1]);
            g[r] = [p[0], p[1], p[2], p[3], q[0], q[1], q[2], q[3]];
        }
        out.push(g);
    }
    Some(out)
}

// ---- the whole surround -------------------------------------------------
/// A frame's chrome: the plate plus this state's four widgets, with no room composed into it.
///
/// Returns the frame as **one palette index per pixel** (the port's `Screen` form): the plate is
/// packed into the screen's own byte image, the widgets are drawn into it byte-addressed, and it is
/// unpacked again. This is what `Game::frame` starts from before it blits the two strips, and what
/// a report can compare against a recorded frame outside the viewports.
pub fn compose(
    raw_trap: &[u8],
    ds: &[u8],
    heat: [i16; 2],
    kit: [KitState; 2],
    secs: u32,
) -> Option<Vec<u8>> {
    let mut bytes = pack(&plate(raw_trap)?);
    for p in 0..2 {
        draw_heat_gauge(&mut bytes, ds, p, heat[p]);
        draw_tray_dots(&mut bytes, ds, p, kit[p].kit);
        draw_kit_box(&mut bytes, ds, p, &kit[p]);
    }
    draw_clock(&mut bytes, ds, secs);
    Some(unpack(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds_img() -> Vec<u8> {
        let exe = std::fs::read(crate::orig::original("spy.exe").expect("spy.exe"))
            .expect("read spy.exe");
        exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000].to_vec()
    }

    fn trap_pic() -> Vec<u8> {
        std::fs::read(crate::orig::original("TRAP.PIC").expect("TRAP.PIC")).expect("read TRAP.PIC")
    }

    /// One of the recon's captured files, used **only** as an oracle: the engine has no loader for
    /// any of them (stage 29). `None` in a checkout that does not have them, and the test skips.
    fn asset(name: &str) -> Option<Vec<u8>> {
        crate::orig::oracle(name)
    }

    /// One pixel out of the screen's byte image.
    fn px(bytes: &[u8], x: usize, y: usize) -> u8 {
        (bytes[y * SCREEN_PITCH + x / 4] >> (6 - 2 * (x % 4))) & 3
    }

    fn blank_bytes() -> Vec<u8> {
        vec![0xFFu8; SCREEN_PITCH * H]
    }

    /// **The plate is the file**, and the captured plate it replaces differs from it by exactly the
    /// 802 widget pixels — no more, and (given what the widgets draw) no less.
    ///
    /// `assets/round_bg.bin` was the modal value over 29,079 recorded frames, so it carries the
    /// widgets' most common pixels. The diff is therefore a *census of the widgets*, and any other
    /// difference would be a decode fault (field order, row order or bit order).
    #[test]
    fn the_plate_is_trap_pic_and_the_802_widget_pixels() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(bg) = asset("round_bg.bin") else {
            crate::orig::skip("assets/round_bg.bin");
            return;
        };
        let got = plate(&trap_pic()).expect("TRAP.PIC decodes");
        let mut diff = 0usize;
        let mut by_rect = [0usize; 6];
        let (mut white_to_cyan, mut magenta_to_black, mut cyan_to_black) = (0usize, 0, 0);
        for y in 0..H {
            for x in 0..W {
                let i = y * W + x;
                if got[i] == bg[i] {
                    continue;
                }
                diff += 1;
                match (got[i], bg[i]) {
                    (3, 1) => white_to_cyan += 1,
                    (2, 0) => magenta_to_black += 1,
                    (1, 0) => cyan_to_black += 1,
                    other => panic!("unexpected change {other:?} at ({x},{y})"),
                }
                let rects = [
                    (GAUGE_X, GAUGE_ROW[0], GAUGE_X + GAUGE_W, GAUGE_ROW[0] + GAUGE_ROWS),
                    (GAUGE_X, GAUGE_ROW[1], GAUGE_X + GAUGE_W, GAUGE_ROW[1] + GAUGE_ROWS),
                    (266, CLOCK_ROW, 310, CLOCK_ROW + CLOCK_ROWS),
                    (100, 74, 240, 74 + DOT_ROWS),
                    (100, 182, 240, 182 + DOT_ROWS),
                    (KIT_X, KIT_ROW[0], KIT_X + KIT_W, KIT_ROW[0] + KIT_ROWS),
                ];
                let mut inside = false;
                for (n, (x0, y0, x1, y1)) in rects.iter().enumerate() {
                    if x >= *x0 && x < *x1 && y >= *y0 && y < *y1 {
                        by_rect[n] += 1;
                        inside = true;
                    }
                }
                assert!(inside, "a differing pixel at ({x},{y}) is outside every widget rect");
            }
        }
        assert_eq!(got.len(), PIXELS);
        assert_eq!(diff, 802, "the captured plate is the file plus the widgets");
        assert_eq!(white_to_cyan, 674, "the two body-heat gauges");
        assert_eq!(magenta_to_black, 112, "the clock's four digit cells");
        assert_eq!(cyan_to_black, 16, "the tray dots' cores");
        assert_eq!(by_rect[0], 358, "the top gauge's drained/remaining rows");
        assert_eq!(by_rect[1], 316, "the bottom gauge's");
        assert_eq!(by_rect[2], 112, "the clock");
        assert_eq!(by_rect[3] + by_rect[4], 16, "the two dot strips");
        assert_eq!(by_rect[5], 0, "the kit box is white in both, as the box stamps it");
    }

    /// **The kit icons are computed, byte for byte** — the three records stamped into the 44 x 10
    /// canvas give exactly the 3 x 11 x 40 icon file the port used to capture, ink counts included.
    #[test]
    fn the_kit_icons_are_computed_from_the_records() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(want) = asset("round_kitbox.bin") else {
            crate::orig::skip("assets/round_kitbox.bin");
            return;
        };
        let ds = ds_img();
        let icons = kit_icons(&ds).expect("the three records parse");
        assert_eq!(icons.len(), 3);
        let ink: Vec<usize> = icons
            .iter()
            .map(|ic| ic.iter().filter(|&&v| v != 255).count())
            .collect();
        assert_eq!(ink, vec![59, 34, 58], "left/middle/right ink pixels");
        for (n, icon) in icons.iter().enumerate() {
            assert_eq!(
                &want[440 * n..440 * (n + 1)],
                &icon[..],
                "kit icon {n} must be the record's own pixels"
            );
        }
    }

    /// **The clock's glyphs are eight-row records**, and the seven-row asset is rows 1..7 of them.
    #[test]
    fn the_clock_glyphs_are_the_eight_row_records() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(want) = asset("round_digits.bin") else {
            crate::orig::skip("assets/round_digits.bin");
            return;
        };
        let ds = ds_img();
        let glyphs = digit_glyphs(&ds).expect("the ten records parse");
        for d in 0..10usize {
            // the asset is rows 1..7 of the record, 8 px a row
            for r in 0..7usize {
                assert_eq!(
                    &want[d * 56 + r * 8..d * 56 + (r + 1) * 8],
                    &glyphs[d][r + 1][..],
                    "digit {d} row {r} (the asset's own row)"
                );
            }
            // and the row the asset is short of is the pod's own colour, so drawing all eight
            // changes nothing on screen
            assert_eq!(glyphs[d][0], [2u8; 8], "digit {d}'s first row is the pod's magenta");
        }
        // the records really are 18 bytes apart, `[8][2]` + 16
        for d in 0..9usize {
            let a = word(&ds, DS_DIGIT_PTR + 2 * d) as usize;
            let b = word(&ds, DS_DIGIT_PTR + 2 * (d + 1)) as usize;
            assert_eq!(b - a, 18, "digit records are 18 bytes apart");
            assert_eq!((ds[a], ds[a + 1]), (8, 2), "`[h=8][w=2]`");
        }
    }

    /// **The gauge is the records' own arithmetic**, not a colour rule: the settled bitmap follows
    /// from the two 14-byte rows and the template, and its 12-px width is what the records stamp.
    #[test]
    fn the_heat_gauge_bitmap_is_the_row_records_own() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let ds = ds_img();
        // the two records' own rows, in the order the parity puts them on: odd bitmap rows take
        // record row 0, even ones take row 1 (`DS:0x15EE` cyan, `DS:0x15FC` white)
        let cap = [0xa8u8, 0x00, 0x02];
        let (cyan0, cyan1) = ([0xa1u8, 0x55, 0x54], [0xa0u8, 0x15, 0x54]);
        let (white0, white1) = ([0xa3u8, 0xff, 0xfc], [0xa0u8, 0x3f, 0xfc]);
        for level in 3..=62i16 {
            let bmp = heat_bitmap(&ds, level).expect("the gauge records parse");
            assert_eq!(bmp.len(), 60 * 3);
            assert_eq!(&bmp[0..3], &cap, "row 0 is the template's cap");
            // the settled rule: white rows 1..L-3, cyan from L-2, parity from the odd bottom row
            let white = (level - 3) as usize;
            for row in 1..GAUGE_ROWS {
                let want = match (row <= white, row % 2 == 1) {
                    (true, true) => white0,
                    (true, false) => white1,
                    (false, true) => cyan0,
                    (false, false) => cyan1,
                };
                assert_eq!(
                    &bmp[row * 3..row * 3 + 3],
                    &want,
                    "level {level}, bitmap row {row} (drained = {})",
                    row <= white
                );
                // the record's own byte 0 and 2: magenta at x 56/57, black at x 67, in both
                assert_eq!(&pixels(bmp[row * 3])[..2], &[2, 2], "x 56/57 are magenta");
                assert_eq!(pixels(bmp[row * 3 + 2])[3], 0, "x 67 is black");
            }
        }
        // and a drawn gauge puts those bytes at x 56..67 of the console's own row
        let mut bytes = blank_bytes();
        assert!(draw_heat_gauge(&mut bytes, &ds, 0, 30));
        assert_eq!(px(&bytes, 56, 8), 2, "the top console starts at row 8");
        assert_eq!(px(&bytes, 67, 9), 0, "x 67 of the stamped rows is the record's black");
        assert_eq!(px(&bytes, 60, 8 + 59), 1, "row 59 is the last one drawn");
        assert!(draw_heat_gauge(&mut bytes, &ds, 1, 30));
        assert_eq!(px(&bytes, 56, 116), 2, "the bottom console at row 116");
    }

    /// The tray dots are the **records'** pixels: four rows each, stamped at the byte columns
    /// `DS:0x2E0E` holds and the row `DS:0x2E0A` holds, held or absent by `DS:0x2D8C+bp`.
    ///
    /// The geometry is the tables', not a list measured off a recording: `bp` = 0x0C .. 0 addresses
    /// `DS:0x2E0E+bp` = 58, 53, 47, 42, 36, 31, 25 (px 232 .. 100, right to left) and
    /// `DS:0x2D8C+bp` = 0x800, 0x20, 0x10, 8, 4, 2, 1. The two record families put their 2x2 core
    /// at a **different offset inside the record** — `+1` for the 4-px boxes (`0x1574`), `+2`/`+3`/
    /// `+4` for the 8-px ones (`0x157A`/`0x1584`/`0x158E`) — because the outline is what moves, and
    /// the core is what stays put: it lands on `items::TRAY_DOT_X`, the ROI the reader scores.
    #[test]
    fn the_tray_dots_are_the_records_at_the_tables_columns() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let ds = ds_img();
        let rpix = |rec: &OpaqueRecord, r: usize, x: usize| pixels(rec.plane[r * rec.bytes + x / 4])[x % 4];
        for (player, row) in [(0usize, 74usize), (1, 182)] {
            for (slot, bp) in (0..=0x0Cu16).step_by(2).rev().enumerate() {
                let col = word(&ds, DS_DOT_COL + bp as usize) as usize;
                let bit = word(&ds, DS_SLOT_BIT + bp as usize);
                let held = OpaqueRecord::at(&ds, word(&ds, DS_DOT_HELD + bp as usize) as usize)
                    .expect("the held record parses");
                let absent = OpaqueRecord::at(&ds, word(&ds, DS_DOT_ABSENT + bp as usize) as usize)
                    .expect("the absent record parses");
                assert_eq!((held.rows, absent.rows), (DOT_ROWS, DOT_ROWS));
                assert_eq!(held.bytes, absent.bytes, "the pair is one box in two states");
                // the core is the 2x2 the held record inks and the absent one leaves empty; its
                // offset inside the record is the outline's, so derive it instead of assuming one
                let (mut cx, mut cy, mut n) = (usize::MAX, usize::MAX, 0usize);
                for r in 0..held.rows {
                    for x in 0..held.bytes * 4 {
                        if rpix(&held, r, x) == 1 && rpix(&absent, r, x) == 0 {
                            n += 1;
                            cx = cx.min(x);
                            cy = cy.min(r);
                        }
                    }
                }
                assert_eq!(n, 4, "slot {slot}: the held core is 2x2");
                assert_eq!(
                    (col * 4 + cx, cy),
                    (crate::items::TRAY_DOT_X[6 - slot], 1),
                    "slot {slot} (bp {bp:#04x}): the core lands on the reader's own ROI"
                );
                for (kit, want) in [(bit, 1u8), (0u16, 0u8)] {
                    let mut bytes = blank_bytes();
                    assert_eq!(draw_tray_dots(&mut bytes, &ds, player, kit), 7);
                    let rec = if kit == 0 { &absent } else { &held };
                    for r in 0..rec.rows {
                        for x in 0..rec.bytes * 4 {
                            assert_eq!(
                                px(&bytes, col * 4 + x, row + r),
                                rpix(rec, r, x),
                                "player {player} slot {slot} (bp {bp:#04x}) record pixel ({r},{x})"
                            );
                        }
                    }
                    assert_eq!(
                        px(&bytes, col * 4 + cx, row + cy),
                        want,
                        "player {player} slot {slot}: the core is the kit bit"
                    );
                }
            }
        }
        // the two tables, as the code walks them
        let cols: Vec<u16> = (0..=0x0Cu16)
            .step_by(2)
            .rev()
            .map(|bp| word(&ds, DS_DOT_COL + bp as usize))
            .collect();
        assert_eq!(cols, vec![58, 53, 47, 42, 36, 31, 25], "right to left");
        assert_eq!(word(&ds, DS_SLOT_BIT), 0x0001, "bp 0 -> the leftmost cell, bit 0");
        assert_eq!(word(&ds, DS_SLOT_BIT + 0x0C), 0x0800, "bp 0x0C -> the map, bit 11");
    }

    /// The kit box's **two passes**, from the words alone: the deposited cell is solid, the carried
    /// one blinks, and neither leaks into the other's cell.
    #[test]
    fn the_kit_box_stamps_the_deposited_and_the_carried_cells_separately() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let ds = ds_img();
        let blank = [255u8; 110];
        // nothing lit, nothing carried
        let st = KitState { kit: 0, carried: 0xFFFF, blink: 1, owner: [0xFFFF; 5] };
        assert_eq!(kit_canvas(&ds, 0, &st).unwrap(), blank.to_vec(), "an empty box is the canvas");
        // the right-hand cell (code 9) deposited by the white spy, kit bit clear
        let mut owner = [0xFFFFu16; 5];
        owner[1] = 0;
        let st = KitState { kit: 0, carried: 0xFFFF, blink: 1, owner };
        let c9 = kit_canvas(&ds, 0, &st).unwrap();
        assert_ne!(c9, blank.to_vec(), "a deposited part is drawn solid");
        assert_eq!(kit_canvas(&ds, 1, &st).unwrap(), blank.to_vec(), "and only for its owner");
        // the left-hand cell (code 11) in the case: on for one blink phase, off for the other
        let st = KitState { kit: 0x200, carried: 11, blink: 1, owner: [0xFFFF; 5] };
        let on = kit_canvas(&ds, 0, &st).unwrap();
        assert_ne!(on, blank.to_vec());
        let st = KitState { blink: -1, ..st };
        assert_eq!(kit_canvas(&ds, 0, &st).unwrap(), blank.to_vec(), "the whole box blinks");
    }

    /// **The clock's digits for every value a round can hold** — the original's own four formulas,
    /// written out as its four instructions, and the four cells they land in.
    ///
    /// The expected digit is derived from `CS:0x229E`'s text, **not** from [`clock_digits`] /
    /// [`CLOCK_COLS`]: reading the pair back off each other is how a reversed column table passed
    /// every test here while drawing a mirror-image clock (found by `frame_check3.py` against a
    /// recorded frame, fixed in this stage).
    #[test]
    fn the_clock_draws_four_eight_row_glyphs_at_the_pods_cells() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let ds = ds_img();
        let glyphs = digit_glyphs(&ds).expect("the ten records parse");
        // the columns, minute tens first: the code's 0x43/0x45/0x49/0x4B, i.e. px 268/276/292/300
        assert_eq!(CLOCK_COLS.map(|c| c * 4), [268, 276, 292, 300]);
        for secs in [0u32, 1, 9, 10, 59, 60, 61, 119, 120, 359, 360, 3599] {
            let mut bytes = blank_bytes();
            assert!(draw_clock(&mut bytes, &ds, secs));
            // `CS:0x229e`: digit(v mod 10) at 0x4B, digit((v/10) mod 6) at 0x49,
            //             digit((v/60) mod 10) at 0x45, digit((v/600) mod 10) at 0x43
            let writes = [
                (0x4B, secs % 10),
                (0x49, (secs / 10) % 6),
                (0x45, (secs / 60) % 10),
                (0x43, (secs / 600) % 10),
            ];
            for (col, digit) in writes {
                let x0 = col * 4;
                let g = glyphs[digit as usize];
                for r in 0..CLOCK_ROWS {
                    for c in 0..8 {
                        assert_eq!(
                            px(&bytes, x0 + c, CLOCK_ROW + r),
                            g[r][c],
                            "secs {secs}: the cell at x {x0} must be digit {digit} (row {r}, col {c})"
                        );
                    }
                }
            }
            // and the ordered form agrees with those four writes, cell for cell: px 268 is the
            // minute tens (`0x43`), px 300 the second units (`0x4B`). The pitch is 8 px except
            // across the colon (276 -> 292).
            let d = clock_digits(secs);
            for (k, col) in CLOCK_COLS.iter().enumerate() {
                assert_eq!(
                    (*col * 4, d[k] as u32),
                    ([268usize, 276, 292, 300][k], [secs / 600, secs / 60, (secs / 10) % 6, secs][k] % 10),
                    "secs {secs}: cell {k} must be digit {} at x {}",
                    d[k],
                    col * 4
                );
            }
        }
    }

    /// The whole surround, composed: plate + widgets, with every widget landing inside its own
    /// rectangle and nothing else touched.
    #[test]
    fn the_composed_surround_only_ever_writes_inside_the_widget_rects() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let ds = ds_img();
        let raw = trap_pic();
        let mut owner = [0xFFFFu16; 5];
        owner[3] = 0;
        let st = [
            KitState { kit: 0x83A, carried: 11, blink: 1, owner },
            KitState { kit: 0x83A, carried: 0xFFFF, blink: -1, owner: [0xFFFF; 5] },
        ];
        let frame = compose(&raw, &ds, [11, 17], st, 359).unwrap();
        let base = plate(&raw).unwrap();
        for y in 0..H {
            for x in 0..W {
                let i = y * W + x;
                if y >= VIEW_TOP.1 && y < VIEW_TOP.1 + crate::round::STRIP_ROWS {
                    continue;
                }
                if y >= VIEW_BOTTOM.1 && y < VIEW_BOTTOM.1 + crate::round::STRIP_ROWS {
                    continue;
                }
                if frame[i] == base[i] {
                    continue;
                }
                assert!(
                    WIDGET_RECTS
                        .iter()
                        .any(|&(x0, y0, x1, y1)| x >= x0 && x < x1 && y >= y0 && y < y1),
                    "a widget wrote outside its rect: ({x},{y})"
                );
            }
        }
    }
}
