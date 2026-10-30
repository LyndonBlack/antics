//! **Stage 28b — the setup selector, drawn by a port of the original's own drawing code.**
//!
//! The selector used to be three captured files (`assets/setup_bg.bin`, `setup_elements.bin`,
//! `setup_rings.bin`) measured off the original running under an emulator. This module derives the
//! same 320x200 frame from the player's own two files — `spy.exe` and `TRAP.PIC` — by porting the
//! routines that draw it (`recon/round/port_check/STAGE27B_SPEC.md`):
//!
//! ```text
//! CS:0x046F   the panel's chrome, once:   the two gauge strips (DS:0x160A, 60 x 3, at byte column
//!             0x0E, rows 8 / 116), the clock (CS:0x229E, four 8 x 2 glyph records out of
//!             DS:0x324F's table at row 0x62), the seven tray dots (CS:0x3278, all held), and the
//!             two kit boxes (CS:0x31C8, kit 0x6C0 / carried 0x0C / blink 1 at byte column 0x0A,
//!             rows 76 / 184)
//! CS:0x0ED1   the selector loop, per state:  a 44 x 58 strip buffer is filled flat 0x55
//!             (CS:0x3445 -> CS:0x33CB, whose tile resolves blank), stamped with the island bitmap
//!             (CS:0x205D), then the launch sign, the two spy figures, the optional `players == 0`
//!             pair, the difficulty bar, the difficulty digit and the GO canvas (CS:0x3455), then
//!             the cursor ring (CS:0x2020); the buffer is blitted to B800 at row 10 / 118, byte
//!             column 20 (CS:0x45B0)
//! ```
//!
//! ## The two record formats (the thing that looked unsolved and was not)
//!
//! * **`CS:0x3455`** takes `[h][w]` + an AND plane + an OR plane, and does `dst = (dst & and) | or`
//!   one destination byte at a time. The five difficulty segments, both launch signs, the GO canvas
//!   and the difficulty bar are *pre-expanded records of exactly that shape*, and together they form
//!   one contiguous block from `DS:0x7330` to `DS:0x7C8A`:
//!
//!   ```text
//!   0x7330 0x7362 0x7394 0x73C6 0x7410   the five difficulty segments  (12 x 2, 12 x 3)
//!   0x7442 0x7624                        HIDE / SHOW  (30 x 8)
//!   0x7806                                the GO canvas (24 x 12)
//!   0x7A48                                the difficulty bar (36 x 8)
//!   ```
//!
//!   which is why each record's next-address arithmetic lands exactly on the next one (482, 578 and
//!   482 bytes respectively). Their AND planes are zero for all but an edge column, so they paint a
//!   black field with white glyphs — the "the payload is zero" in stage 27b was the *AND* plane.
//! * **`CS:0x0E8C` + `CS:0x3455`** — the two spy figures live in the run-time sprite banks and are
//!   *packed*: `[h][w]` + `h*w` bytes, expanded through the 256-byte LUTs at `DS:0x70C0` (AND) and
//!   `DS:0x71C0` (OR) into the two-plane shape before the blit. Bank 1 is `DS:0xB360` (segment
//!   `0x327B`, the figures both states draw), bank 2 is `DS:0xE860` (segment `0x327D`, the
//!   `players == 0` pair, drawn mirrored: mode 1 walks the row right to left and reverses each
//!   byte's four 2-bit pixels).
//!
//! ## The coordinate asymmetry, which is real
//!
//! `CS:0x3455` adds 2 to the destination byte index; `CS:0x2020` and `CS:0x205D` are entered with
//! `DI = DS:0x1A90` and do not. So in strip-buffer byte terms a masked record lands at `cx`, the
//! island bitmap at `cx - 2`, and a ring at `xbyte - 2`. "Fixing" that shifts the island and every
//! ring 8 px.
//!
//! ## Verified
//!
//! All **21 recorded selector states** (7 islands, 2 launch signs, 2 player counts, 5 difficulty
//! levels, 5 cursor zones) reproduce **pixel-exact**, and the derived frame with no ring is
//! **byte-identical** to the tracked `assets/setup_bg.bin`. See `tests` at the bottom and
//! `recon/round/port_check/STAGE28B_REPORT.md`.

use std::path::Path;

use crate::chrome::{self, KitState, OpaqueRecord, SCREEN_PITCH};
use crate::PIXELS;

// ---- the player's files --------------------------------------------------
/// `DS:0 == file 0x400` (the load base is paragraph 0x1000, `DS` is 0x1020).
pub const DS_FILE_OFF: usize = 0x400;
/// The data segment the port models: 64 KB, so the sprite banks at `DS:0xE860` are inside.
pub const DS_BYTES: usize = 0x10000;
/// `TRAP.PIC`: 16,192 bytes read straight into the CGA frame buffer (`CS:0x29F3`).
pub const TRAP_PIC_BYTES: usize = 0x3F40;
pub const EXE_NAME: &str = "spy.exe";
pub const TRAP_PIC_NAME: &str = "TRAP.PIC";

// ---- the panel's chrome (CS:0x046F) --------------------------------------
/// The gauge template `DS:0x160A`: `[60][3]`, blitted untouched (nothing on this screen reads a
/// heat level), so both BODY HEAT bars are full.
pub const DS_GAUGE_TEMPLATE: usize = 0x160A;
/// `DS:0x2F5C` / `DS:0x2F5E` — the two row indices, 8 and 116.
pub const DS_GAUGE_ROW: usize = 0x2F5C;
pub const GAUGE_ROWS: usize = 60;
pub const GAUGE_BYTES: usize = 3;
pub const GAUGE_COL: usize = 0x0E;
/// `CS:0x04CB`: `DS:0x324D = 1`, and `CS:0x229E` decrements before it draws — so the panel's clock
/// reads **00:00**, not the round's countdown.
pub const PANEL_CLOCK_SECS: u32 = 0;
/// `CS:0x0476` / `CS:0x0482`: the tray dots are drawn with the kit word forced to `0xFFF`, i.e. all
/// seven held, on both consoles.
pub const PANEL_TRAY_KIT: u16 = 0xFFF;
/// `CS:0x04D4` / `CS:0x04EC`: what the panel hands `CS:0x31C8` for each console — nothing in hand
/// (`0x0C`), the kit display full-ish (`0x6C0`), and the carried set blinking (`1`).
pub const PANEL_KIT_WORD: u16 = 0x6C0;
pub const PANEL_CARRIED: u16 = 0x0C;
pub const PANEL_BLINK: i16 = 1;

// ---- the strip buffer (CS:0x0ED1) ----------------------------------------
/// `DS:0x1A90` — the buffer's header is `[rows = 58][stride = 44]`, then 58 x 44 bytes.
pub const STRIP_ROWS: usize = 58;
/// `DS:0x1A92`'s second byte.
pub const STRIP_BYTES: usize = 44;
/// `DS:0x24A2` / `DS:0x24A4` — the row-table indices of the two blits (rows 10 and 118).
pub const BLIT_ROW: [usize; 2] = [10, 118];
/// `CS:0x10BB`: `CX = 0x14`, a byte column, i.e. x = 80.
pub const BLIT_COL: usize = 20;
/// `CS:0x3445` -> `CS:0x33CB` resolves both selector calls to page tile 20, which is blank: the
/// plate is a uniform `0x55` (cyan). Every byte of both strips is accounted for by this fill plus
/// the records drawn over it.
pub const PLATE: u8 = 0x55;

/// `DS:0x72C0` — seven words: island -> its `[45][22]` bitmap (`CS:0x205D` copies it).
pub const DS_ISLAND: usize = 0x72C0;
/// The island's plain copy: `BX = 0x33` (bottom row), `CX = 0x0D` (byte column).
pub const ISLAND_BOTTOM: usize = 0x33;
pub const ISLAND_COL: usize = 0x0D;

/// `DS:0x31EB` — two words: `0x7442` (HIDE) / `0x7624` (SHOW), the pre-expanded 30 x 8 records.
pub const DS_LAUNCH: usize = 0x31EB;
pub const LAUNCH_BOTTOM: usize = 0x1E;
pub const LAUNCH_COL: usize = 0x13;
/// `DS:0x31E1` — five words: the pre-expanded difficulty segments, bottom row 0x24, byte column 0x26.
pub const DS_DIFFICULTY: usize = 0x31E1;
pub const DIFFICULTY_BOTTOM: usize = 0x24;
pub const DIFFICULTY_COL: usize = 0x26;
/// `DS:0x7A48` — the difficulty bar, a pre-expanded 36 x 8 record.
pub const DS_BAR: usize = 0x7A48;
pub const BAR_BOTTOM: usize = 0x2A;
pub const BAR_COL: usize = 0x23;
/// `DS:0x7806` — the GO canvas, a pre-expanded 24 x 12 record.
pub const DS_GO: usize = 0x7806;
pub const GO_BOTTOM: usize = 0x38;
pub const GO_COL: usize = 0x11;
/// Segment `0x327B` — the two spy figures both states draw, **packed** (`CS:0x0E8C` expands them).
pub const DS_SPY_BANK: usize = 0xB360;
pub const SPY_A_BOTTOM: usize = 0x23;
pub const SPY_B_OFFSET: usize = 0x9A;
pub const SPY_B_BOTTOM: usize = 0x2C;
/// Segment `0x327D` — the `players == 0` pair, same two records, drawn mirrored at byte column 6.
pub const DS_PAIR_BANK: usize = 0xE860;
pub const PAIR_COL: usize = 0x06;
/// `DS:0x70C0` / `DS:0x71C0` — the 256-byte AND / OR expansion LUTs of `CS:0x0E8C`.
pub const DS_MASK_LUT: usize = 0x70C0;
pub const DS_DATA_LUT: usize = 0x71C0;
/// `DS:0x31EF` / `DS:0x31F9` — the five ring rectangles, indexed by the cursor value itself:
/// `(y << 8) | bytes` and `(x byte << 8) | rows`.
pub const DS_RING_YW: usize = 0x31EF;
pub const DS_RING_XH: usize = 0x31F9;

/// One selector state. `cursor = None` is "no ring", which is what `setup_bg.bin` is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    /// `DS:0x2EC0`: 0 island, 2 launch, 4 players, 6 difficulty, 8 GO!.
    pub cursor: Option<u8>,
    /// `DS:0x31BB / 2`, 0..6.
    pub island: usize,
    /// `DS:0x31D7 / 2`: 0 HIDE, 1 SHOW.
    pub launch: usize,
    /// `DS:0x31DB`: 0 = one spy, 1 = both (the port's own field, as `Selector::players` holds it).
    pub players: usize,
    /// `DS:0x31BF / 2`, 0..4 — the digit 1..5.
    pub difficulty: usize,
}

impl Default for State {
    fn default() -> State {
        State { cursor: Some(0), island: 0, launch: 0, players: 0, difficulty: 0 }
    }
}

/// The player's two files, and the frame they draw.
pub struct Panel {
    ds: Vec<u8>,
    trap: Vec<u8>,
}

fn word(ds: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*ds.get(off)?, *ds.get(off + 1)?]))
}

impl Panel {
    /// Build a panel from a `spy.exe` image and a `TRAP.PIC`, refusing anything too short to hold
    /// the tables this module reads.
    pub fn from_files(spy_exe: &[u8], trap: &[u8]) -> Option<Panel> {
        if spy_exe.len() < DS_FILE_OFF + DS_BYTES || trap.len() < TRAP_PIC_BYTES {
            return None;
        }
        let ds = spy_exe[DS_FILE_OFF..DS_FILE_OFF + DS_BYTES].to_vec();
        Some(Panel { ds, trap: trap[..TRAP_PIC_BYTES].to_vec() })
    }

    /// The same, from a directory holding `spy.exe` and `TRAP.PIC` (the originals, not `assets/`).
    /// `None` when either file is missing, so a caller can fall back to whatever it did before.
    pub fn load(dir: &Path) -> Option<Panel> {
        let exe = std::fs::read(dir.join(EXE_NAME)).ok()?;
        let trap = std::fs::read(dir.join(TRAP_PIC_NAME)).ok()?;
        Panel::from_files(&exe, &trap)
    }

    /// The data segment as this panel sees it — the tests read tables back through it.
    #[cfg(test)]
    pub fn ds(&self) -> &[u8] {
        &self.ds
    }

    /// The selector frame for `st`: 320x200, one palette index per pixel (CGA palette 1).
    pub fn render(&self, st: &State) -> Option<Vec<u8>> {
        let trap = chrome::decode_trap_pic(&self.trap)?;
        let mut scr = chrome::pack(&trap);
        self.chrome(&mut scr)?;
        let (top, bot) = self.strips(st)?;
        blit(&mut scr, &top, BLIT_ROW[0]);
        blit(&mut scr, &bot, BLIT_ROW[1]);
        let px = chrome::unpack(&scr);
        debug_assert_eq!(px.len(), PIXELS);
        Some(px)
    }

    /// **`CS:0x046F`** — the panel's chrome, drawn once before the selector's loop, in the
    /// original's own order: the tray dots (`0x47F`, `0x48B`), the two gauge blits (`0x4AE`,
    /// `0x4BC`), the clock (`0x4D1`), then the two kit boxes (`0x4E9`, `0x501`).
    fn chrome(&self, scr: &mut [u8]) -> Option<()> {
        let ds = &self.ds;
        // CS:0x3278 — the seven tray dots, all held, on both consoles (the panel writes 0xFFF
        // into DS:0x32A3 / DS:0x32A5 before each call).
        for p in 0..2usize {
            chrome::draw_tray_dots(scr, ds, p, PANEL_TRAY_KIT);
        }
        // CS:0x21BD without a heat step: the template itself, 60 rows x 3 bytes, at byte column
        // 0x0E, on the row each console's DS:0x2F5C / DS:0x2F5E word names (8 / 116).
        let gauge = OpaqueRecord {
            rows: GAUGE_ROWS,
            bytes: GAUGE_BYTES,
            plane: ds.get(DS_GAUGE_TEMPLATE + 2..DS_GAUGE_TEMPLATE + 2 + GAUGE_ROWS * GAUGE_BYTES)?
                .to_vec(),
        };
        for p in 0..2usize {
            let row = word(ds, DS_GAUGE_ROW + 2 * p)? as i32;
            gauge.copy_to(scr, SCREEN_PITCH, row, GAUGE_COL as i32);
        }
        // CS:0x229E — the clock, at 00:00 (its own column order, minute tens first).
        if !chrome::draw_clock(scr, ds, PANEL_CLOCK_SECS) {
            return None;
        }
        // CS:0x31C8 — the two kit boxes, with the panel's own three words.
        let mut owner = [0u16; 5];
        for (k, o) in owner.iter_mut().enumerate() {
            *o = word(ds, chrome::DS_PART_DEPOSIT - 2 + 2 * k)?;
        }
        let st = KitState {
            kit: PANEL_KIT_WORD,
            carried: PANEL_CARRIED,
            blink: PANEL_BLINK,
            owner,
        };
        for p in 0..2usize {
            if !chrome::draw_kit_box(scr, ds, p, &st) {
                return None;
            }
        }
        Some(())
    }

    /// **`CS:0x0ED1`** — the two strip buffers, in the original's own draw order.
    fn strips(&self, st: &State) -> Option<(Strip, Strip)> {
        let ds = &self.ds;
        let mut top = Strip::new();
        let mut bot = Strip::new();
        // 1. island art — CS:0x205D, a plain copy at strip (row 6, byte 11).
        let island_addr = word(ds, DS_ISLAND + 2 * st.island)? as usize;
        top.plain(ds, island_addr, ISLAND_BOTTOM, ISLAND_COL);
        // 2. the launch sign (HIDE / SHOW) — CS:0x3455 at (row 0, byte 19).
        bot.masked(ds, word(ds, DS_LAUNCH + 2 * st.launch)? as usize,
                   LAUNCH_BOTTOM, LAUNCH_COL, Mode::Normal);
        // 3./4. the two spy figures, always: packed records through CS:0x0E8C's LUTs.
        bot.masked(ds, DS_SPY_BANK, SPY_A_BOTTOM, 0, Mode::Expand);
        bot.masked(ds, DS_SPY_BANK + SPY_B_OFFSET, SPY_B_BOTTOM, 0, Mode::Expand);
        // 5./6. the pair, only with two spies (`DS:0x31DB == 0`), mirrored at byte column 6.
        if st.players == 1 {
            bot.masked(ds, DS_PAIR_BANK, SPY_A_BOTTOM, PAIR_COL, Mode::MirrorExpand);
            bot.masked(ds, DS_PAIR_BANK + SPY_B_OFFSET, SPY_B_BOTTOM, PAIR_COL, Mode::MirrorExpand);
        }
        // 7. the difficulty bar, 8. its digit, 9. the GO canvas.
        bot.masked(ds, DS_BAR, BAR_BOTTOM, BAR_COL, Mode::Normal);
        bot.masked(ds, word(ds, DS_DIFFICULTY + 2 * st.difficulty)? as usize,
                   DIFFICULTY_BOTTOM, DIFFICULTY_COL, Mode::Normal);
        bot.masked(ds, DS_GO, GO_BOTTOM, GO_COL, Mode::Normal);
        // 10. the cursor ring — into the top buffer for zone 0, the bottom one otherwise.
        if let Some(cur) = st.cursor {
            let yw = word(ds, DS_RING_YW + cur as usize)?;
            let xh = word(ds, DS_RING_XH + cur as usize)?;
            let dst = if cur == 0 { &mut top } else { &mut bot };
            dst.ring(yw >> 8, yw & 0xFF, xh >> 8, xh & 0xFF);
        }
        Some((top, bot))
    }
}

/// Which blitter shape a record needs: `CS:0x3455` as-is, `CS:0x0E8C`'s LUT expansion first, or the
/// mirrored path (`CS:0x3559`) with or without the expansion.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Expand,
    MirrorExpand,
}

/// A 44 x 58 strip buffer, i.e. `DS:0x1A92`'s data (the two header bytes are not modelled).
struct Strip {
    b: Vec<u8>,
}

impl Strip {
    /// The plate: a uniform `0x55` (see [`PLATE`]).
    fn new() -> Strip {
        Strip { b: vec![PLATE; STRIP_ROWS * STRIP_BYTES] }
    }

    fn put(&mut self, row: isize, col: isize, v: u8) {
        if row < 0 || col < 0 || row >= STRIP_ROWS as isize || col >= STRIP_BYTES as isize {
            return;
        }
        self.b[row as usize * STRIP_BYTES + col as usize] = v;
    }

    fn get(&self, row: isize, col: isize) -> u8 {
        if row < 0 || col < 0 || row >= STRIP_ROWS as isize || col >= STRIP_BYTES as isize {
            return 0;
        }
        self.b[row as usize * STRIP_BYTES + col as usize]
    }

    /// **`CS:0x205D`** — an opaque bitmap; dest row `bottom - h`, dest byte `cx - 2`.
    fn plain(&mut self, ds: &[u8], addr: usize, bottom: usize, cx: usize) {
        let rows = ds[addr] as usize;
        let bytes = ds[addr + 1] as usize;
        let row0 = bottom as isize - rows as isize;
        for r in 0..rows {
            for i in 0..bytes {
                self.put(row0 + r as isize, cx as isize - 2 + i as isize,
                         ds[addr + 2 + r * bytes + i]);
            }
        }
    }

    /// **`CS:0x3455`** — `dst = (dst & and) | or`, at dest row `bottom - h`, dest byte `cx`
    /// (the +2 of `CS:0x346f` is this module's 0, because the buffer here starts after the header).
    fn masked(&mut self, ds: &[u8], addr: usize, bottom: usize, cx: usize, mode: Mode) {
        let rows = ds[addr] as usize;
        let bytes = ds[addr + 1] as usize;
        let n = rows * bytes;
        let (and, or) = match mode {
            Mode::Normal => {
                (ds[addr + 2..addr + 2 + n].to_vec(), ds[addr + 2 + n..addr + 2 + 2 * n].to_vec())
            }
            Mode::Expand | Mode::MirrorExpand => {
                // CS:0x0E8C: packed bytes -> the two planes through the 256-byte LUTs.
                let lut_a = &ds[DS_MASK_LUT..DS_MASK_LUT + 256];
                let lut_o = &ds[DS_DATA_LUT..DS_DATA_LUT + 256];
                let packed = &ds[addr + 2..addr + 2 + n];
                (packed.iter().map(|&b| lut_a[b as usize]).collect(),
                 packed.iter().map(|&b| lut_o[b as usize]).collect())
            }
        };
        let row0 = bottom as isize - rows as isize;
        for r in 0..rows {
            for i in 0..bytes {
                // CS:0x3559: the row is written right to left with each byte's four 2-bit pixels
                // reversed, i.e. the sprite is mirrored.
                let mirrored = mode == Mode::MirrorExpand;
                let col = if mirrored {
                    cx as isize + (bytes - 1 - i) as isize
                } else {
                    cx as isize + i as isize
                };
                let k = r * bytes + i;
                let (a, o) = if mirrored {
                    (crate::round::reverse_pixels(and[k]), crate::round::reverse_pixels(or[k]))
                } else {
                    (and[k], or[k])
                };
                let d = self.get(row0 + r as isize, col);
                self.put(row0 + r as isize, col, (d & a) | o);
            }
        }
    }

    /// **`CS:0x2020`** — a 1-px black outline `wbytes * 4` px wide and `hrows` tall, at dest row
    /// `y`, dest byte `xbyte - 2`.
    fn ring(&mut self, y: u16, wbytes: u16, xbyte: u16, hrows: u16) {
        let (y, wb, x0, h) = (y as isize, wbytes as isize, xbyte as isize - 2, hrows as isize);
        for c in 0..wb {
            self.put(y, x0 + c, 0);
            self.put(y + h - 1, x0 + c, 0);
        }
        for r in (y + 1)..(y + h - 1) {
            let l = self.get(r, x0) & 0x3F;
            self.put(r, x0, l);
            let right = x0 + wb - 1;
            let v = self.get(r, right) & 0xFC;
            self.put(r, right, v);
        }
    }
}

/// **`CS:0x45B0`** — copy one strip buffer to the screen: 58 scanlines, 44 bytes each, from screen
/// row `row0` at byte column [`BLIT_COL`]. The original's jump table of 58 unrolled copiers and its
/// `0x2000`/`0x1FB0` bank deltas only exist because it writes the CGA interlace directly; here the
/// screen is already de-interleaved.
fn blit(scr: &mut [u8], strip: &Strip, row0: usize) {
    for r in 0..STRIP_ROWS {
        let d = (row0 + r) * SCREEN_PITCH + BLIT_COL;
        let s = r * STRIP_BYTES;
        scr[d..d + STRIP_BYTES].copy_from_slice(&strip.b[s..s + STRIP_BYTES]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orig;
    use crate::W;

    fn panel() -> Panel {
        let dir = orig::require("spy-vs-spy-iii/original/256");
        Panel::load(&dir).expect("the selector's two files")
    }

    /// One of the recon's captured files, used **only** as an oracle: the engine has no loader for
    /// any of them (stage 29), and a checkout without them skips the comparison.
    fn asset(name: &str) -> Option<Vec<u8>> {
        orig::oracle(name)
    }

    fn diff(a: &[u8], b: &[u8]) -> (usize, Option<(usize, usize, usize, usize)>) {
        let mut n = 0;
        let (mut x0, mut y0, mut x1, mut y1) = (W, crate::H, 0, 0);
        for i in 0..a.len().min(b.len()) {
            if a[i] != b[i] {
                n += 1;
                let (x, y) = (i % W, i / W);
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        (n, if n == 0 { None } else { Some((x0, y0, x1, y1)) })
    }

    /// **The derived frame is the tracked background, byte for byte.**
    ///
    /// `setup_bg.bin` is a captured selector screen with the five rings lifted out; this module
    /// draws the same picture from `spy.exe` + `TRAP.PIC` alone. 0 of 64,000 px may differ.
    #[test]
    fn the_derived_frame_is_the_tracked_background() {
        if orig::absent() {
            orig::skip("the selector's two files");
            return;
        }
        let Some(bg) = asset("setup_bg.bin") else {
            orig::skip("assets/setup_bg.bin (the selector oracle)");
            return;
        };
        let got = panel().render(&State { cursor: None, ..State::default() }).expect("a frame");
        let (n, box_) = diff(&got, &bg);
        assert_eq!(n, 0, "differing pixels {n} at {box_:?}");
    }

    /// **Every element state is the tracked element art on the tracked background.**
    ///
    /// `setup_elements.bin` stores, per option group, the rectangle that ever varies and one image
    /// per value; painting value *v* through the group's mask onto the background has to equal this
    /// module's frame for that value. That covers all 7 islands, both launch signs, both player
    /// counts and all five difficulty levels.
    #[test]
    fn every_element_value_matches_the_tracked_elements() {
        if orig::absent() {
            orig::skip("the selector's element art");
            return;
        }
        let (Some(bg), Some(blob)) = (asset("setup_bg.bin"), asset("setup_elements.bin")) else {
            orig::skip("assets/setup_bg.bin + setup_elements.bin (the element oracle)");
            return;
        };
        let p = panel();
        let rd16 = |o: usize| u16::from_le_bytes([blob[o], blob[o + 1]]);
        let count = rd16(0) as usize;
        let mut o = 2usize;
        let mut checked = 0;
        for _ in 0..count {
            let nlen = blob[o] as usize;
            o += 1;
            let name = std::str::from_utf8(&blob[o..o + nlen]).expect("a name");
            o += nlen;
            let x = i16::from_le_bytes([blob[o], blob[o + 1]]);
            let y = i16::from_le_bytes([blob[o + 2], blob[o + 3]]);
            let w = rd16(o + 4) as usize;
            let h = rd16(o + 6) as usize;
            o += 8;
            let nvalues = blob[o] as usize;
            o += 1;
            let nbits = (w * h).div_ceil(8);
            let mask: Vec<bool> = (0..w * h)
                .map(|i| blob[o + i / 8] & (0x80 >> (i % 8)) != 0)
                .collect();
            o += nbits;
            for v in 0..nvalues {
                let mut want = bg.clone();
                let img = &blob[o..o + w * h];
                o += w * h;
                for r in 0..h {
                    for c in 0..w {
                        if mask[r * w + c] {
                            let i = (y as usize + r) * W + x as usize + c;
                            want[i] = img[r * w + c];
                        }
                    }
                }
                let mut st = State { cursor: None, ..State::default() };
                match name {
                    "island" => st.island = v,
                    "launch" => st.launch = v,
                    "players" => st.players = v,
                    "difficulty" => st.difficulty = v,
                    other => panic!("unknown element group {other}"),
                }
                let got = p.render(&st).expect("a frame");
                let (n, box_) = diff(&got, &want);
                assert_eq!(n, 0, "{name}[{v}]: differing pixels {n} at {box_:?}");
                checked += 1;
            }
        }
        assert_eq!(checked, 16, "7 islands + 2 signs + 2 player counts + 5 levels");
    }

    /// **Every ring zone is the tracked ring on the tracked background.**
    ///
    /// `setup_rings.bin` stores, per cursor zone, the rectangle and the pixels that differ between
    /// two cursor positions. Rendering the same state from `DS:0x31EF` / `DS:0x31F9` has to produce
    /// exactly those pixels, and nothing outside them.
    #[test]
    fn every_ring_zone_matches_the_tracked_rings() {
        if orig::absent() {
            orig::skip("the selector's ring art");
            return;
        }
        let Some(blob) = asset("setup_rings.bin") else {
            orig::skip("assets/setup_rings.bin (the ring oracle)");
            return;
        };
        let p = panel();
        let count = blob[0] as usize;
        let mut o = 1usize;
        for _ in 0..count {
            let zone = blob[o];
            let x = i16::from_le_bytes([blob[o + 1], blob[o + 2]]);
            let y = i16::from_le_bytes([blob[o + 3], blob[o + 4]]);
            let w = u16::from_le_bytes([blob[o + 5], blob[o + 6]]) as usize;
            let h = u16::from_le_bytes([blob[o + 7], blob[o + 8]]) as usize;
            o += 9;
            let nbits = (w * h).div_ceil(8);
            let mask: Vec<bool> = (0..w * h)
                .map(|i| blob[o + i / 8] & (0x80 >> (i % 8)) != 0)
                .collect();
            o += nbits;
            let img = &blob[o..o + w * h];
            o += w * h;
            let got = p.render(&State { cursor: Some(zone), ..State::default() }).expect("a frame");
            let mut painted = 0usize;
            for r in 0..h {
                for c in 0..w {
                    if !mask[r * w + c] {
                        continue;
                    }
                    let i = (y as usize + r) * W + x as usize + c;
                    assert_eq!(got[i], img[r * w + c],
                               "zone {zone}: pixel ({}, {})", x as usize + c, y as usize + r);
                    painted += 1;
                }
            }
            // and nothing outside the ring: every other pixel is the ring-less frame
            let plain = p.render(&State { cursor: None, ..State::default() }).expect("a frame");
            let (n, box_) = diff(&got, &plain);
            assert_eq!(n, painted, "zone {zone}: {n} px differ from the ring-less frame");
            if let Some((x0, y0, x1, y1)) = box_ {
                assert!(x0 >= x as usize && x1 <= x as usize + w - 1
                        && y0 >= y as usize && y1 <= y as usize + h - 1,
                        "zone {zone}: a diff at {box_:?} is outside the rect");
            }
        }
        assert_eq!(count, 5, "five cursor zones");
    }

    /// **The plate really is a uniform `0x55`.** Every pixel of the two strip rectangles that no
    /// record draws is palette index 1 — that is what makes the background a decode and not art.
    #[test]
    fn the_strip_plate_is_a_uniform_cyan_fill() {
        if orig::absent() {
            orig::skip("the selector's two files");
            return;
        }
        let p = panel();
        let got = p.render(&State { cursor: None, ..State::default() }).expect("a frame");
        // The island is the only thing the default state puts in the top strip; its rectangle is
        // the record's own header, at (bottom 0x33 - h, byte 0x0D - 2).
        let ds = p.ds();
        let addr = word(ds, DS_ISLAND).expect("the island table") as usize;
        let (ih, iw) = (ds[addr] as usize, ds[addr + 1] as usize);
        let (irow, icol) = (ISLAND_BOTTOM - ih, ISLAND_COL - 2);
        let mut fill_px = 0usize;
        for row0 in BLIT_ROW {
            for r in 0..STRIP_ROWS {
                for c in 0..STRIP_BYTES {
                    if row0 == BLIT_ROW[0] && (irow..irow + ih).contains(&r)
                        && (icol..icol + iw).contains(&c)
                    {
                        continue;
                    }
                    if row0 == BLIT_ROW[1] {
                        continue;      // the bottom strip is all records; the asserts above cover it
                    }
                    let i = (row0 + r) * W + (BLIT_COL + c) * 4;
                    for k in 0..4 {
                        assert_eq!(got[i + k], 1, "strip row {r} byte {c} px {k} is not the fill");
                        fill_px += 1;
                    }
                }
            }
        }
        assert_eq!(fill_px, (STRIP_ROWS * STRIP_BYTES - ih * iw) * 4,
                   "the top strip is the island plus the fill");
    }

    /// **Nothing here is captured: change a byte in the player's file and the frame follows.**
    ///
    /// Three probes, one per kind of table: a clock glyph (`DS:0x7004`), the island bitmap
    /// (`DS:0x7DAA`), and the difficulty bar's OR plane (`DS:0x7C6A`). Each must move the pixels
    /// this module says it moves, and only those.
    #[test]
    fn every_table_this_module_reads_comes_from_the_file() {
        if orig::absent() {
            orig::skip("the selector's two files");
            return;
        }
        let dir = orig::require("spy-vs-spy-iii/original/256");
        let exe = std::fs::read(dir.join(EXE_NAME)).expect("spy.exe");
        let trap = std::fs::read(dir.join(TRAP_PIC_NAME)).expect("TRAP.PIC");
        let base = Panel::from_files(&exe, &trap).expect("a panel");
        let before = base.render(&State { cursor: None, ..State::default() }).expect("a frame");

        // The panel's clock reads 00:00, so the *same* '0' glyph record is drawn in all four cells:
        // one byte of its fifth row (0x7004 + 2 + 4*2) must move 4 px in each of the four cells and
        // nothing else on the screen.
        let mut exe2 = exe.clone();
        exe2[DS_FILE_OFF + 0x7004 + 2 + 8] ^= 0xFF;
        let after = Panel::from_files(&exe2, &trap).expect("a panel")
            .render(&State { cursor: None, ..State::default() }).expect("a frame");
        let (n, box_) = diff(&after, &before);
        assert_eq!(box_, Some((268, 102, 303, 102)),
                   "the clock probe must move only the clock's four cells on row 102, not {box_:?}");
        assert_eq!(n, 16, "one flipped byte is four pixels in each of the four 00:00 cells");

        // the island bitmap's first payload byte -> strip (row 6, byte 11) = screen x 124, y 16
        let mut exe3 = exe.clone();
        exe3[DS_FILE_OFF + 0x7DAA + 2] ^= 0xFF;
        let after = Panel::from_files(&exe3, &trap).expect("a panel")
            .render(&State { cursor: None, ..State::default() }).expect("a frame");
        let (n, box_) = diff(&after, &before);
        assert!(n > 0, "the island bitmap must reach the screen");
        let (x0, y0, x1, y1) = box_.expect("a diff");
        assert_eq!((x0, y0, x1, y1), (124, 16, 127, 16), "the island's first pixel row");

        // the bar's OR plane -> the difficulty box in the bottom strip (x 232, y 142)
        let mut exe4 = exe.clone();
        exe4[DS_FILE_OFF + 0x7A48 + 2 + 288 + 12] ^= 0xFF;
        let after = Panel::from_files(&exe4, &trap).expect("a panel")
            .render(&State { cursor: None, ..State::default() }).expect("a frame");
        let (n, box_) = diff(&after, &before);
        assert!(n > 0, "the bar's OR plane must reach the screen");
        let (x0, y0, x1, y1) = box_.expect("a diff");
        assert!(x0 >= 228 && x1 <= 260 && y0 >= 118 && y1 <= 154, "the bar probe moved {box_:?}");
    }

    /// **The player count only moves the pair.** With two spies the second figure appears at byte
    /// column 6 of the bottom strip and nothing else changes.
    #[test]
    fn the_second_figure_is_the_only_thing_the_player_count_moves() {
        if orig::absent() {
            orig::skip("the selector's two files");
            return;
        }
        let p = panel();
        let one = p.render(&State { cursor: None, players: 0, ..State::default() }).expect("a frame");
        let two = p.render(&State { cursor: None, players: 1, ..State::default() }).expect("a frame");
        let (n, box_) = diff(&two, &one);
        assert!(n > 0, "the pair must draw something");
        let (x0, y0, x1, y1) = box_.expect("a diff");
        assert!(x0 >= BLIT_COL * 4 + 6 * 4 && x1 < BLIT_COL * 4 + 14 * 4
                && y0 >= BLIT_ROW[1] + 16 && y1 < BLIT_ROW[1] + 45,
                "the pair moved {n} px in {box_:?}, outside its own cells");
    }

    /// **The whole state grid renders, deterministically, and each option changes its own cell.**
    ///
    /// 7 islands x 5 cursor zones (35) plus 5 difficulty levels x 2 signs x 2 player counts (20) —
    /// the spec's cheap subset of the 700-state grid.
    #[test]
    fn the_whole_option_grid_renders() {
        if orig::absent() {
            orig::skip("the selector's two files");
            return;
        }
        let p = panel();
        let mut frames = Vec::new();
        for island in 0..7usize {
            for cursor in [0u8, 2, 4, 6, 8] {
                let st = State { cursor: Some(cursor), island, ..State::default() };
                let a = p.render(&st).expect("a frame");
                let b = p.render(&st).expect("the same frame");
                assert!(a == b, "island {island} cursor {cursor} is not deterministic");
                frames.push((st, a));
            }
        }
        assert_eq!(frames.len(), 35);
        // the seven islands must all differ from one another
        for i in 1..7 {
            let (_, a) = &frames[i * 5];
            let (_, b) = &frames[0];
            assert!(a != b, "island {i} draws the same picture as island 0");
        }
        for difficulty in 0..5usize {
            for launch in 0..2usize {
                for players in 0..2usize {
                    let st = State { cursor: Some(6), island: 0, launch, players, difficulty };
                    assert!(p.render(&st).is_some(), "difficulty {difficulty} launch {launch}");
                }
            }
        }
    }
}
