//! The round view: the 176x58 room viewport, composed from `spy.exe` plus a game-state struct.
//!
//! Everything here is table-driven from the original's bytes at run time. No captures, no baked
//! pixels: the art bank, the plate table, the level table, the gate table and the object blocks are
//! all read out of the executable image and the data-segment dump.
//!
//! ## What the original does (all of this is read off the disassembly, see the notes per function)
//!
//! ```text
//! plate[ level[cell] ]  --blit (CX, DX=0, BX)-->  DS:0x1A92 strip (44 B x 58 rows)
//!                                             --copy-->  screen at (80,10)  and (80,118)
//!                                             --then-->  objects, then the spy sprite
//! ```
//!
//! * `FUN_26c0_33cb` (CS:0x33CB) is the plate blitter. It switches DS to the art segment and copies
//!   `min(26 - CX, 44 - DX)` bytes per row, 58 rows, advancing the source by `26 - n` and the
//!   destination by `44 - n`; then `DX += n`, `BX += 2`, `CX = 0` and it goes round again until
//!   `DX > 43`. That is the up-to-three-pass stitch: for `CX = 11` the widths are 15, 26, 3.
//! * The caller at CS:0x0B18 picks `CX` and `BX`: it scans the gate table at DS:0x25AE (0, 26, 52,
//!   ... 286, i.e. the 26-byte cell boundaries of the world row) from the right until
//!   `mapX >= gate[col]`, so `CX = mapX - gate[col]` and `col = mapX / 26`; then
//!   `BX = row_base[band] + 2*col`, where `row_base` is DS:0x24AA (0, 24, 48, ... 168).
//! * `level = word[DS:0x24BE + BX]`, and `plate = word[DS:0x25C6 + level]` — an **unscaled byte
//!   offset** into the plate table, so only even level values are meaningful.
//! * `mapX` is a **byte** column in a 312-byte world row (12 cells x 26 bytes). 312 - 44 = 268 =
//!   0x10C is the clamp the round loop applies, which is the arithmetic proof of that reading.
//!
//! ## Segment arithmetic (measured, and it corrects one constant that circulates in the notes)
//!
//! `file offset = 0x200 + (segment - 0x1000) * 16`, because the load base is paragraph 0x1000:
//! DS is segment 0x1020 (so `DS:0 == file 0x400`), the code segment is 0x26C0 (so `CS:0` is file
//! 0x16E00, which is why Ghidra's functions are named `FUN_26c0_xxxx`), and the art segment 0x11F6
//! is **file 0x12160**. The same arithmetic puts the two spy banks at file 0x39A0 (segment 0x137A,
//! white) and 0x6EA0 (segment 0x16CA, black).
//!
//! ## The two sprite layers (stage 2A)
//!
//! Both layers are drawn by the same masked blitter (`FUN_26c0_3455`) but feed it **different
//! record formats**, and getting that wrong is what kept stage 1 open:
//!
//! * **objects** — `DS:0x2D38[code]` (or the shared `DS:0x6F80` for every bit-7 code) points at
//!   `[height][width_bytes]` followed by **two planes**, mask then data, already in final 2bpp
//!   form. The blitter uses them raw: `and [di],al`, then `or [di],al`.
//! * **spy** — the record in the player's bank (`DS:0x327B` -> segment 0x137A white / 0x16CA black)
//!   is `[height][width_bytes]` followed by **one plane** whose pixel value 1 means transparent.
//!   `FUN_26c0_0e8c` expands it into a two-plane buffer through `DS:0x70C0` (AND) and `DS:0x71C0`
//!   (OR), *both indexed by the same source byte* — which is why `0x55` is a fully transparent
//!   byte and why the spy planes are full of it.
//!
//! Two blits per spy: the body with its bottom row on `Y-9` and the legs overlay on `Y`; both 8
//! bytes wide at `x = [0x329B] - mapX`; facing left is a per-row reversal of the four 2-bit pixels
//! *and* of the byte order, which is exactly the blitter's `[0x31AE] == 1` branch.
//!
//! The pose -> record map is **derived** from the two byte-offset tables (`DS:0x2622` and
//! `DS:0x2628`); see [`PoseParams`]. Stage 1 had it as an observed three-entry map and could not
//! cover anything but the plain walk.
//!
//! ## Measured on the recorded population
//!
//! Over 1082 dumps from 6 sessions (`recon/round/port_check/STAGE2A_REPORT.md`), the composed strip
//! is **byte-identical to the strip the game itself left in the dump for 881 of them (81.4%)**,
//! with a median residual of 0 bytes; every miss is a dump caught in the middle of the round loop's
//! own draw sequence (a stale plate band, or the strip as it stands just before the spy pass).
//!
//! The sprite layers stay off whenever `DS:0x2F7E` is set for that player: the object routine
//! returns on that flag at its first instruction, and measured over the population those strips are
//! the plates alone.

#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::terrain::Terrain;
use crate::Screen;

/// `ANTICS_HIDE_ORIGINAL=1` under `cargo test` makes the player's own files invisible — the same
/// answer a public checkout gets. Outside `cfg(test)` there is no `orig` module and nothing to ask.
fn hidden_for_tests() -> bool {
    #[cfg(test)]
    {
        crate::orig::hidden()
    }
    #[cfg(not(test))]
    {
        false
    }
}

// ---- the strip -----------------------------------------------------------
pub const STRIP_LEN: usize = 44 * 58; // 2552 bytes, 176 x 58 px at 2 bpp
pub const STRIP_ROWS: usize = 58;
pub const STRIP_PITCH: usize = 44;
/// Top console's viewport in the 320x200 frame (measured: 0 of 10208 pixels differ).
pub const VIEW_TOP: (usize, usize) = (80, 10);
/// Bottom console's viewport (measured: same composition, 0 of 10208 px, at this offset).
pub const VIEW_BOTTOM: (usize, usize) = (80, 118);

// ---- file offsets of the two segments we read ----------------------------
/// Art segment 0x11F6. The value stored in the code is a **relocated** segment (the loader adds the
/// load base 0x1000), so the run-time segment is 0x21F6 and the file offset is
/// `0x200 + 0x11F6*16 = 0x12160`. Measured: composing from here reproduces recorded strips exactly.
pub const ART_FILE_OFF: usize = 0x200 + 0x11F6 * 16; // 0x12160
/// White spy's bank, segment 0x137A.
pub const BANK_WHITE_OFF: usize = 0x200 + (0x137A - 0x1000) * 16; // 0x39A0
/// Black spy's bank, segment 0x16CA.
pub const BANK_BLACK_OFF: usize = 0x200 + (0x16CA - 0x1000) * 16; // 0x6EA0
/// DS image: `DS:0 == file 0x400`.
pub const DS_FILE_OFF: usize = 0x400;

// ---- data-segment addresses (all read from the dump, never assumed) ------
pub const DS_MAP_X: usize = 0x24A6;
pub const DS_ROW_BASE: usize = 0x24AA;
pub const DS_LEVELS: usize = 0x24BE;
pub const DS_GATE: usize = 0x25AE;
pub const DS_PLATE: usize = 0x25C6;
pub const DS_STRIP: usize = 0x1A92;
pub const DS_BAND: usize = 0x329F; // per-player: +0 for player 1, +2 for player 2
/// `DS:0x324D` — the round's remaining seconds as the countdown keeps it (measured: 0x167 = 359 on
/// a 360 s island round, one second in).
pub const DS_CLOCK_SECS: usize = 0x324D;
pub const DS_SPY_X: usize = 0x329B;
pub const DS_SPY_Y: usize = 0x3297;
pub const DS_POSE: usize = 0x2F0C;
pub const DS_MIRROR: usize = 0x2F14;
pub const DS_BLANK: usize = 0x2F7E;
pub const DS_BLOCK_PTR: usize = 0x2600;
pub const DS_OBJ_ROW: usize = 0x2612; // 34, 40, 48, 56: sprite bottom row per wrapped row
pub const DS_OBJ_TABLE: usize = 0x2D38;
pub const DS_OBJ_FIXED: usize = 0x6F80; // shared record for every bit-7 code
pub const DS_AND_TAB: usize = 0x70C0;
pub const DS_OR_TAB: usize = 0x71C0;
/// The spy's sprite bank, per player (`0x137A` white, `0x16CA` black).
pub const DS_SPY_BANK: usize = 0x327B;
/// The pose-set selector: 0x24 (36) is the plain walk, 0x6C (108) the walk carrying an item, and
/// so on. It is added into the record-table index, so it selects a whole block of poses.
pub const DS_POSE_SET: usize = 0x2EF8;
/// Walk frame: 2 -> the first legs overlay of the pair, 4 -> the second (the animation alternates).
pub const DS_POSE_FRAME: usize = 0x2EFC;
/// Extra byte offset into the record table (0 for player 1 in every dump examined).
pub const DS_POSE_EXTRA: usize = 0x32AB;
/// `DS:0x2F2C` (+2) — the carried item code (`0xFFFF` = nothing). Stage 7 owns the semantics;
/// `round.rs` needs the address because one value of it (`7`, the snowshoes) changes the sprite.
pub const DS_CARRIED: usize = 0x2F2C;
/// `DS:0x32A3` (+2) — the kit word (the trapulator's tool bits plus the part mirrors).
pub const DS_KIT: usize = 0x32A3;
/// `word[pose]` -> a row offset into the record table.
pub const DS_POSE_ROW: usize = 0x2622;
/// `DS:0x2F2C` (+2) — `7` shifts the legs row by 0x12. Every other value in the recordings is
/// `0xFFFF` (the word is used as a "unset" sentinel), and only `== 7` matters.
pub const DS_POSE_ROW_SHIFT: usize = 0x2F2C;
/// The value the recordings hold when the row shift is *not* active.
pub const POSE_ROW_SHIFT_OFF: u16 = 0xFFFF;
/// `word[row + frame + extra]` -> the record's offset from `SPRITE_BASE`.
pub const DS_POSE_TABLE: usize = 0x2628;
/// The record-table entries are offsets from here: the first body record (`0x7DC0`, dir 0).
pub const SPRITE_BASE: u16 = 0x7DC0;
/// `[p+0x2F64] >= 2` skips the whole spy draw (the code's own early-out).
pub const DS_SPY_STATE: usize = 0x2F64;
/// `DS:0x2F58` (+2) — body heat, counted as cold (stage 14).
pub const DS_HEAT: usize = 0x2F58;
/// `DS:0x2F60` (+2) — the per-terrain rate counter (stage 14).
pub const DS_HEAT_T: usize = 0x2F60;
/// `DS:0x2F54` (+2) — the base-drain counter (stage 14).
pub const DS_HEAT_BASE: usize = 0x2F54;
/// The "raised" branch selector: non-zero for climbing/entering poses, where the body's `sub $9`
/// is skipped and the body sits on `Y` instead of `Y-9`.
pub const DS_POSE_RAISED: usize = 0x2F18;
/// The pose's own sub-state; `0x0C` and `0x14` arm the vertical clip offset.
pub const DS_POSE_STATE: usize = 0x32CF;
/// The vertical clip/scroll offset (added to the destination row on the raised branch).
pub const DS_POSE_CLIP: usize = 0x2F68;
/// `word[0x2CC4 + state]` — the pose set an **animation state** switches to (`CS:27e9`). The table
/// is indexed by the state word as a **byte** offset, the same way `0x2CC0` is; entry `0x0C` holds
/// `0xDC`, the laugh, and every state `>= 0x1E` and `0x1C` is never read (those states keep the
/// spy's own set). Measured on the recorded hatch sequences: the other spy's set is `0xDC` on the
/// first frame of state `0x0C` and back to `0x24` when it ends.
pub const DS_ANIM_SET: usize = 0x2CC4;
/// `word[0x2D22 + 0x2EFC]` — the legs table state `0x1C` reads instead of its fixed `+2`
/// (`CS:e2d`): `0, 2, 4, 2, 0, 2, 4, 2, 0` indexed by the frame word, i.e. the **walk cycle**.
/// That is the owner's "just as they land the legs do the walk animation": the body stands still
/// (state `>= 0x1C` never adds the frame to the body record) while only the legs cycle.
pub const DS_LEGS_WALK: usize = 0x2D22;
/// Why the spy draw has two clip constants: the body's record is shifted down by the clip until it
/// passes the body's own `sub $9` (`CS:d2e` compares the clip with 9), then by `clip - 9` with the
/// bottom rows dropped (`0x31B2` in the blitter, `CS:346a`).
pub const BODY_CLIP: i32 = 9;
/// The legs are skipped once the clip passes the six rows of clearance the code tests (`CS:dc9`).
pub const LEGS_CLIP_MAX: i32 = 6;

/// One player's room-view inputs. The original's per-player variables are interleaved: player 1's
/// fields sit at `base + 0`, player 2's at `base + 2` (`0x24A6`/`0x24A8`, `0x329F`/`0x32A1`, ...).
#[derive(Clone, Copy, Debug)]
pub struct Spy {
    pub x: u16,
    pub y: u16,
    pub pose: u16,
    pub mirror: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Player {
    pub map_x: u16,
    pub band: u16,
    pub spy: Spy,
    /// `DS:0x2F7E` — non-zero means the round loop used the fixed-argument blanking call
    /// (`CS:0x1A245`: band 0, BX 0x78, CX 0, DX 0) instead of the gate-based room blit.
    pub blank: bool,
}

/// Everything the room view is a function of. Parsed out of a 64 KB data-segment dump, so the
/// numbers are the running game's own; the executable supplies the tables and the art.
#[derive(Clone, Debug)]
pub struct RoundState {
    /// `DS:0x329F` — band selector of the player this state was read for (2, 4 or 6).
    pub screen_id: u16,
    /// `DS:0x24A6` — byte column of the viewport's left edge in the 312-byte world row.
    pub map_x: u16,
    /// `DS:0x24BE` — the level table: **120 words**, i.e. 10 rows of 12. The island record's
    /// `+0x1C` pointer supplies all 120 (stage 2B measured 120 words; the row count was stage 2B's
    /// 5, which is half of it). Rows 5..9 are the igloo interiors — level 20 is the flat interior
    /// plate and 18/22 the two interior variants — and a room at screen 14 or 18 selects row 7 or 9,
    /// so a 5-row table cannot compose an interior at all.
    pub levels: [[u16; 12]; 10],
    /// `DS:0x24AA` — 8 row bases (0, 24, 48, ...).
    pub row_base: [u16; 8],
    /// `DS:0x25AE` — 12 gate values (0, 26, 52, ... 286).
    pub gate: [u16; 12],
    /// `DS:0x25C6` — 26 plate offsets into the art segment.
    pub plate: [u16; 26],
    /// `DS:0x2600` — pointers to the five 624-byte object blocks.
    pub block_ptr: [u16; 5],
    /// The five blocks themselves, copied out of `DS:0x53BC..`.
    pub blocks: Vec<u8>,
    pub players: [Player; 2],
    /// The raw dump, so the object layer and `--check` can read whatever they need.
    pub ds: Vec<u8>,
}

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// A word read through a 16-bit segment register: the second byte wraps from 0xFFFF to 0x0000,
/// which is what the original does when a computed offset runs off the end of the segment.
fn ds_word(ds: &[u8], addr: u16) -> u16 {
    let a = addr as usize;
    let lo = ds[a];
    let hi = ds[(a + 1) & 0xFFFF];
    u16::from_le_bytes([lo, hi])
}

impl RoundState {
    pub fn from_ds_dump(path: &Path) -> Result<RoundState, String> {
        let ds = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        RoundState::from_bytes(ds)
    }

    pub fn from_bytes(ds: Vec<u8>) -> Result<RoundState, String> {
        if ds.len() < 0x10000 {
            return Err(format!(
                "a data-segment dump must be 65536 bytes, this is {}",
                ds.len()
            ));
        }
        let mut levels = [[0u16; 12]; 10];
        for r in 0..10 {
            for c in 0..12 {
                levels[r][c] = u16le(&ds, DS_LEVELS + 2 * (r * 12 + c));
            }
        }
        let mut row_base = [0u16; 8];
        for (i, v) in row_base.iter_mut().enumerate() {
            *v = u16le(&ds, DS_ROW_BASE + 2 * i);
        }
        let mut gate = [0u16; 12];
        for (i, v) in gate.iter_mut().enumerate() {
            *v = u16le(&ds, DS_GATE + 2 * i);
        }
        let mut plate = [0u16; 26];
        for (i, v) in plate.iter_mut().enumerate() {
            *v = u16le(&ds, DS_PLATE + 2 * i);
        }
        let mut block_ptr = [0u16; 5];
        for (i, v) in block_ptr.iter_mut().enumerate() {
            *v = u16le(&ds, DS_BLOCK_PTR + 2 * i);
        }
        // the five blocks are contiguous, 0x270 (624) bytes apart, starting at the first pointer
        let base = block_ptr[0] as usize;
        let blocks = ds[base..(base + 5 * 624).min(ds.len())].to_vec();

        let mut players = [Player {
            map_x: 0,
            band: 0,
            spy: Spy { x: 0, y: 0, pose: 0, mirror: false },
            blank: false,
        }; 2];
        for (i, p) in players.iter_mut().enumerate() {
            let o = 2 * i;
            p.map_x = u16le(&ds, DS_MAP_X + o);
            p.band = u16le(&ds, DS_BAND + o);
            p.spy = Spy {
                x: u16le(&ds, DS_SPY_X + o),
                y: u16le(&ds, DS_SPY_Y + o),
                pose: u16le(&ds, DS_POSE + o),
                mirror: u16le(&ds, DS_MIRROR + o) != 0,
            };
            p.blank = u16le(&ds, DS_BLANK + o) != 0;
        }
        let screen_id = players[0].band;
        Ok(RoundState {
            screen_id,
            map_x: players[0].map_x,
            levels,
            row_base,
            gate,
            plate,
            block_ptr,
            blocks,
            players,
            ds,
        })
    }

    /// The level value the blit will use for `player`'s leftmost cell — useful in reports.
    pub fn left_level(&self, player: usize) -> u16 {
        let p = &self.players[player];
        let col = (self.gate_col(p.map_x)) as usize;
        self.levels[(p.band as usize / 2).saturating_sub(1)][col]
    }

    fn gate_col(&self, map_x: u16) -> u16 {
        let mut col = 11u16;
        while col > 0 && map_x < self.gate[col as usize] {
            col -= 1;
        }
        col
    }
}

// ---- the art segment -----------------------------------------------------
/// The 64 KB art segment (segment 0x11F6), with the part past end-of-file zero-filled. 37843 of
/// the 65536 bytes ship in the image; every one of the 26 plate offsets is inside that range.
pub struct Art {
    pub seg: Vec<u8>,
    pub static_len: usize,
}

impl Art {
    pub fn from_exe(exe: &[u8]) -> Result<Art, String> {
        if exe.len() <= ART_FILE_OFF {
            return Err(format!(
                "spy.exe is {} bytes, too short for the art segment at {ART_FILE_OFF:#x}",
                exe.len()
            ));
        }
        let raw = &exe[ART_FILE_OFF..];
        let static_len = raw.len().min(0x10000);
        let mut seg = vec![0u8; 0x10000];
        seg[..static_len].copy_from_slice(&raw[..static_len]);
        Ok(Art { seg, static_len })
    }

    pub fn plate(&self, off: u16) -> &[u8] {
        let o = off as usize & 0xFFFF;
        let end = (o + 26 * 58).min(self.seg.len());
        &self.seg[o..end]
    }
}

// ---- the plate blit (FUN_26c0_33cb) -------------------------------------
/// One pass of the blitter, as a report line.
#[derive(Clone, Copy, Debug)]
pub struct Pass {
    pub cell_off: u16,
    pub level: u16,
    pub plate: u16,
    pub cx: u16,
    pub dx: u16,
    pub width: u16,
}

/// Replay `FUN_26c0_33cb` for one player. Returns the 44x58 strip and the passes it took.
pub fn compose_passes(
    art: &Art,
    st: &RoundState,
    player: usize,
) -> ([u8; STRIP_LEN], Vec<Pass>) {
    let p = &st.players[player];
    let mut out = [0u8; STRIP_LEN];
    let mut passes = Vec::with_capacity(3);

    // The blanking call (CS:0x1A245) has fixed arguments: band 0, BX 0x78, CX 0, DX 0.
    let (mut bx, mut cx) = if p.blank {
        (0x78u16, 0u16)
    } else {
        let col = st.gate_col(p.map_x);
        let cx = p.map_x.wrapping_sub(st.gate[col as usize]);
        // `[bp+0x24aa]` with bp a BYTE offset -- the game reads the word at 0x24AA + band, so a band
        // past the table's 8 entries reads past it rather than clamping. (Recorded dumps contain
        // band = 18, and clamping there costs every byte of the strip.)
        let row_base = ds_word(&st.ds, (DS_ROW_BASE as u16).wrapping_add(p.band));
        (row_base.wrapping_add(2 * col), cx)
    };

    let mut dx = 0u16;
    loop {
        let level = ds_word(&st.ds, DS_LEVELS as u16 + bx);
        let plate = ds_word(&st.ds, DS_PLATE as u16 + level);
        let mut n = 0x1Au16.wrapping_sub(cx);
        if 0x2C < dx.saturating_add(n) {
            n = 0x2C - dx;
        }
        passes.push(Pass { cell_off: bx, level, plate, cx, dx, width: n });
        let src = art.plate(plate);
        for row in 0..STRIP_ROWS {
            let d = dx as usize + row * STRIP_PITCH;
            let s = cx as usize + row * 26;
            for i in 0..n as usize {
                if s + i < src.len() && d + i < STRIP_LEN {
                    out[d + i] = src[s + i];
                }
            }
        }
        dx += n;
        if 0x2B < dx {
            break;
        }
        bx = bx.wrapping_add(2);
        cx = 0;
    }
    (out, passes)
}

/// The room viewport for the state's own player (the one `map_x`/`screen_id` came from).
pub fn compose_view(exe: &[u8], st: &RoundState) -> [u8; STRIP_LEN] {
    match Art::from_exe(exe) {
        Ok(art) => compose_passes(&art, st, 0).0,
        Err(_) => [0u8; STRIP_LEN],
    }
}

pub fn compose_view_for(exe: &[u8], st: &RoundState, player: usize) -> [u8; STRIP_LEN] {
    match Art::from_exe(exe) {
        Ok(art) => compose_passes(&art, st, player).0,
        Err(_) => [0u8; STRIP_LEN],
    }
}

// ---- pixels -------------------------------------------------------------
/// The CGA palette the round screen uses: 0 black, 1 cyan, 2 magenta, 3 white.
pub const ROUND_PALETTE: [[u8; 3]; 4] = [
    [0x00, 0x00, 0x00],
    [0x55, 0xFF, 0xFF],
    [0xFF, 0x55, 0xFF],
    [0xFF, 0xFF, 0xFF],
];

/// Unpack a 2bpp strip to one palette index per pixel, 176 wide x 58 tall, row-major.
pub fn strip_to_indices(strip: &[u8; STRIP_LEN]) -> Vec<u8> {
    let mut idx = vec![0u8; 176 * STRIP_ROWS];
    for row in 0..STRIP_ROWS {
        for b in 0..STRIP_PITCH {
            let byte = strip[row * STRIP_PITCH + b];
            for k in 0..4 {
                idx[row * 176 + b * 4 + k] = (byte >> (6 - 2 * k)) & 3;
            }
        }
    }
    idx
}

/// Unpack a 2bpp strip to RGB triples, 176 * 58 of them, row-major.
pub fn strip_to_rgb(strip: &[u8; STRIP_LEN]) -> Vec<[u8; 3]> {
    strip_to_indices(strip)
        .into_iter()
        .map(|i| ROUND_PALETTE[i as usize])
        .collect()
}

// ---- the masked blitter (FUN_26c0_3455), for the object and spy layers --
/// A record the blitter can consume: `[height][width_bytes]` then two planes of `h*w` bytes.
/// The blitter does `dest = (dest & AND) | OR` per byte, so an AND byte of `0xFF` keeps the
/// destination and an AND byte of `0` lets the OR plane through.
pub struct Record<'a> {
    pub height: usize,
    pub width: usize,
    pub and: Vec<u8>,
    pub or: Vec<u8>,
    _src: &'a [u8],
}

impl<'a> Record<'a> {
    /// The same parse as [`Record::expand_planes`] but with **owned** planes and no borrow, so a
    /// caller can build one from a data segment it just assembled (the rocket lives in `ds`, which
    /// `round_state` rebuilds every frame).
    pub fn owned(src: &[u8]) -> Option<Record<'static>> {
        let r = Record::expand_planes(src)?;
        Some(Record { height: r.height, width: r.width, and: r.and, or: r.or, _src: &[] })
    }

    /// A record from two planes the caller already has (the rocket's and the flame's own form,
    /// which carries no expansion tables at all).
    pub fn from_planes(height: usize, width: usize, and: Vec<u8>, or: Vec<u8>)
        -> Record<'static>
    {
        Record { height, width, and, or, _src: &[] }
    }

    /// **Object records, as the object layer blits them.** `[h][w]` then a mask plane and a data
    /// plane of `h*w` bytes each, used verbatim: the blitter at CS:0x3455 does `and [di],al` with
    /// the first plane's byte and `or [di],al` with the second's, with no table in between. The
    /// records at `DS:0x2D38[code]` are stored in exactly this form -- 2bpp pixels throughout, the
    /// mask plane holding 0 (opaque) or 3 (transparent) per pixel -- so the 0x70C0/0x71C0 tables,
    /// which expand a *self-masked* byte whose pixel value 1 means transparent, must NOT be applied
    /// here. That mix-up is what made stage 1's objects land as black silhouettes.
    pub fn expand_planes(src: &'a [u8]) -> Option<Record<'a>> {
        if src.len() < 4 {
            return None;
        }
        let (h, w) = (src[0] as usize, src[1] as usize);
        if h == 0 || w == 0 || h > 64 || w > 32 {
            return None;
        }
        let n = h * w;
        if src.len() < 2 + 2 * n {
            return None;
        }
        Some(Record {
            height: h,
            width: w,
            and: src[2..2 + n].to_vec(),
            or: src[2 + n..2 + 2 * n].to_vec(),
            _src: src,
        })
    }

    /// **Spy records.** `[h][w]` then a *single* 2bpp plane of `h*w` bytes: four pixels per byte,
    /// MSB first, and a pixel value of **1 means transparent**. Both expansion tables
    /// (`DS:0x70C0` -> AND, `DS:0x71C0` -> OR) are indexed by that same source byte, which is why
    /// `0x55` (four 1s) is a fully transparent byte and why the spy planes are peppered with it.
    /// `DS:0x0E8C` is the routine that does this expansion into a scratch buffer before the blit.
    pub fn expand_selfmasked(ds_img: &[u8], src: &'a [u8]) -> Option<Record<'a>> {
        if src.len() < 4 {
            return None;
        }
        let (h, w) = (src[0] as usize, src[1] as usize);
        if h == 0 || w == 0 || h > 64 || w > 32 {
            return None;
        }
        let n = h * w;
        if src.len() < 2 + n {
            return None;
        }
        let plane = &src[2..2 + n];
        Some(Record {
            height: h,
            width: w,
            and: plane.iter().map(|&b| ds_img[DS_AND_TAB + b as usize]).collect(),
            or: plane.iter().map(|&b| ds_img[DS_OR_TAB + b as usize]).collect(),
            _src: src,
        })
    }

    /// Legacy two-plane read through the expansion tables (stage 1's model). Kept because it is
    /// part of the module's public surface; **neither layer uses it** -- the tables are for the
    /// spy's single self-masking plane, and the object records are already two raw planes.
    pub fn expand(ds_img: &[u8], src: &'a [u8]) -> Option<Record<'a>> {
        if src.len() < 4 {
            return None;
        }
        let h = src[0] as usize;
        let w = src[1] as usize;
        if h == 0 || w == 0 || h > 64 || w > 32 || src.len() < 2 + 2 * h * w {
            return None;
        }
        let n = h * w;
        let and = (0..n).map(|i| ds_img[DS_AND_TAB + src[2 + i] as usize]).collect();
        let or = (0..n).map(|i| ds_img[DS_OR_TAB + src[2 + n + i] as usize]).collect();
        Some(Record { height: h, width: w, and, or, _src: src })
    }
}

/// Draw a record into the strip with the original's clipping. `x` and `bottom` are in strip bytes
/// and rows; `mirror` reverses each row's pixels (which is what the blitter's reverse branch does).
pub fn masked_blit(
    strip: &mut [u8; STRIP_LEN],
    rec: &Record,
    x: i32,
    bottom: i32,
    mirror: bool,
) {
    masked_blit_clipped(strip, rec, x, bottom, mirror, 0)
}

/// The same blit with the original's `DS:0x31B2` row offset (`CS:3461..34ca`):
///
/// ```text
/// top  = bottom - rows + shift
/// if (top < 0) { skip = -top; top = 0 }     ; CS:346f, the screen's top edge
/// rows -= shift + skip                      ; CS:34a0/34ba
/// ```
///
/// so the sprite moves **down** by `shift` and the rows that no longer fit at the bottom are not
/// drawn. `FUN_26c0_0c93` uses it for the hatch: the descent's clip is 2,4,..,28, which is what
/// makes the spy sink into the hatch mouth instead of sliding a copy of himself down the room.
pub fn masked_blit_clipped(
    strip: &mut [u8; STRIP_LEN],
    rec: &Record,
    x: i32,
    bottom: i32,
    mirror: bool,
    shift: i32,
) {
    let shift = shift.max(0);
    let mut top = bottom - rec.height as i32 + shift;
    let mut skip = 0i32;
    if top < 0 {
        skip = -top;
        top = 0;
    }
    let rows = rec.height as i32 - shift - skip;
    for r in 0..rows.max(0) {
        let y = top + r;
        if y < 0 || y >= STRIP_ROWS as i32 {
            continue;
        }
        let src_row = skip + r;
        if src_row < 0 || src_row >= rec.height as i32 {
            continue;
        }
        for c in 0..rec.width {
            let mut a = rec.and[src_row as usize * rec.width + c];
            let mut o = rec.or[src_row as usize * rec.width + c];
            let xb = if mirror { x + (rec.width - 1 - c) as i32 } else { x + c as i32 };
            if xb < 0 || xb >= STRIP_PITCH as i32 {
                continue;
            }
            if mirror {
                a = reverse_pixels(a);
                o = reverse_pixels(o);
            }
            let i = y as usize * STRIP_PITCH + xb as usize;
            strip[i] = (strip[i] & a) | o;
        }
    }
}

/// Reverse the four 2-bit pixels in a byte (the blitter's per-row mirror, proved for all 256
/// values: 0x1B -> 0xE4, 0xC8 -> 0x23).
pub fn reverse_pixels(b: u8) -> u8 {
    ((b & 0x03) << 6) | ((b & 0x0C) << 2) | ((b & 0x30) >> 2) | ((b & 0xC0) >> 6)
}

// ---- the map (the trapulator's slot 12, `FUN_26c0_2b7f`) ----------------
//
// The map is **not the trapulator panel**. Pressing A on slot 12 takes `CS:2A4E`'s map branch,
// which sets `DS:0x2F48+si` and jumps to `CS:2B6E` — the panel's own close — so the panel is
// *down* for the whole look. The window is drawn later in the same frame, from the per-spy draw
// loop at `CS:0BAA` (`if (0x2f48 != 0) call 0x2b7f`), into the console's viewport work buffer
// (`DS:0x1A90`, 176 x 58 at 44 bytes a row) and blitted to the screen with the rest of the scene.
//
// Everything below is the routine's own arithmetic, cross-checked against the recorded strips:
// `recon/round/port_check/STAGE18C_REPORT.md` §2.
//
// ```text
// 2b7f  bx = si ^ 2                                  ; the OTHER spy
// 2b85  if (word[0x2f64+bx] == 0 && word[0x2f7e] + word[0x2f80] != 0) { 0x2f48 = 0; return }
// 2b9e  if (word[0x2f68+si] > 6) { 0x2f48 = 0; return }
// 2bae  the plate: di = 0x1a90, si = 0x1052, bx = 0x32, cx = 0x0d, dx = 0x2c, call 0x205d
// 2bc4  for di in (6,4,2,0):  the four legs of the spy's trail
// 2c60  for di in (8,6,4,2):  the four position records (`DS:0x3219`)
// 2ca3  if (0x2f48 >= 0)      the blinking own-position marker
// 2cd5  negw 0x2f48(%si)                             ; the blink
// ```

/// `DS:0x1052` — the **plate** the draw routine copies into the viewport: 88 x 45 four-colour
/// pixels, 22 bytes a row, behind a two-byte header `[45][22]` (`0x205D`'s `[top row][width]`;
/// the caller's `bx = 0x32` (50) minus that 45 is the five rows the window sits down from the top
/// of the viewport).
pub const DS_MAP_PLATE: usize = 0x1052;
/// The whole blob: header + 45 rows of 22 bytes = 992 bytes, the exact `cx` of the round-start
/// copy (`CS:121C`: `mov $0x3e0,%cx; rep movsb`).
pub const MAP_PLATE_LEN: usize = 0x3E0;
pub const MAP_PLATE_ROWS: usize = 45;
pub const MAP_PLATE_BYTES: usize = 22;
/// `DS:0x72C0` — **seven words**, one per island, each the file/segment offset of that island's
/// plate blob. This is the whole of the plate's "composition": `CS:1204` reads
/// `bx = word[0x31BB]` (the island's byte offset: 0, 2, ... 12), then
/// `si = word[0x72C0+bx]`, `di = 0x1052`, `cx = 0x3E0`, `rep movsb`. The seven blobs sit 0x3E0
/// apart, so the table is a plain array of pictures and the artwork is island-specific — which is
/// why `DS:0x1052` is (almost) blank in the image and why lifting one island's plate for another
/// is wrong.
pub const DS_ISLAND_PLATE: usize = 0x72C0;
/// Where `CS:2BAE` puts the plate in the viewport: `di = 0x1A92 + 11 + 44*5` (the `cx = 0x0d`
/// byte column plus the `50 - 45` rows), i.e. 88 x 45 at (44, 5) px of a 176 x 58 strip.
pub const MAP_ROW: usize = 5;
pub const MAP_COL: usize = 11;
/// `DS:0x15D8` — the map lattice's **five rows**, in strip rows: 48, 40, 32, 24, 16.
/// `DS:0x15E2` — its **six columns**, in strip bytes: 13, 16, 19, 22, 25, 28 (12 px apart).
/// Both are read with the index the code computes (a byte index into the words, so only even
/// indices mean anything — exactly as in the original).
pub const MAP_ROWS_TAB: usize = 0x15D8;
pub const MAP_COLS_TAB: usize = 0x15E2;
/// `DS:0x3313` — per spy, the pointer to his five-entry **trail** (`0x2DF2` white, `0x2DFC`
/// black): the last five map cells he has stood in, newest first. `CS:26F7` fills all five with
/// the round-start cell; `CS:36D1` shifts the word in when the cell changes.
pub const DS_MAP_TRAIL_PTR: usize = 0x3313;
pub const DS_MAP_TRAIL: usize = 0x2DF2;
/// `DS:0x32FF` (+2) — the spy's map column: the gate column of his scroll window, rounded down to
/// even (`CS:3689`: `ax = di >> 1; and $0xfffe`), i.e. 0, 2, ... 10 — the six lattice columns.
pub const DS_MAP_COL: usize = 0x32FF;
/// `DS:0x2F48` (+2) — the map's own display word. Non-zero = up; the **sign** is the blink phase
/// (`CS:2CD5` negates it once per drawn frame) and the own-position marker is drawn only while it
/// is non-negative.
pub const DS_MAP_FLAG: usize = 0x2F48;
/// `DS:0x3219` (+di) / `DS:0x320F` (+di) — the position record of the four carried codes 9..12:
/// the screen it lies in (`>= 0`) and its column inside that screen's own row.
pub const DS_POS_RECORD: usize = 0x3219;
pub const DS_POS_COL: usize = 0x320F;
/// The marker records, all in the `[h][w] + and plane + or plane` form `0x3455` blits.
/// `0x6190` and `0x61CE` are the two ends of a **long** leg of the trail, `0x61AE` and `0x61E4`
/// the two ends of a **short** one (`CS:2BFB`: `if (span <= 2)` picks 6/2 else 4/0 out of the
/// selector table at `0x1484`). `0x148C` is the "a part is here" dot, `0x1432` the 9-row
/// "you are here" marker.
pub const MAP_REC_LEG_LONG: [u16; 2] = [0x61CE, 0x6190];
pub const MAP_REC_LEG_SHORT: [u16; 2] = [0x61E4, 0x61AE];
pub const MAP_REC_PART: u16 = 0x148C;
pub const MAP_REC_OWN: u16 = 0x1432;
/// `CS:2BFB` — a leg of two cells or less draws the short pair.
pub const MAP_LEG_SHORT_MAX: u16 = 2;
/// `CS:2C7A` — one map column is 0x1A (26) entries of the position record's own row.
pub const MAP_COL_STRIDE: u8 = 0x1A;

/// The island's plate blob, `[rows][width]` then 45 rows of 22 bytes (`CS:1204`'s source).
pub fn island_plate(exe: &[u8], island: usize) -> Option<&[u8]> {
    if island > 6 {
        return None;
    }
    let ds = exe.get(DS_FILE_OFF..DS_FILE_OFF + 0x10000)?;
    let at = u16le(ds, DS_ISLAND_PLATE + 2 * island) as usize;
    ds.get(at..at + MAP_PLATE_LEN)
}

/// `CS:1204` — the round-start copy of the island's plate into `DS:0x1052`.
pub fn materialise_map_plate(ds: &mut [u8], ds_img: &[u8], island: usize) {
    if island > 6 || ds_img.len() < 0x10000 || ds.len() < DS_MAP_PLATE + MAP_PLATE_LEN {
        return;
    }
    let at = u16le(ds_img, DS_ISLAND_PLATE + 2 * island) as usize;
    if at + MAP_PLATE_LEN > ds_img.len() {
        return;
    }
    ds[DS_MAP_PLATE..DS_MAP_PLATE + MAP_PLATE_LEN]
        .copy_from_slice(&ds_img[at..at + MAP_PLATE_LEN]);
}

/// One map lattice entry: `word[base + index]`, refusing an index the original would run past
/// the table with (the code does no bounds test at all; a trail value or a record that lands
/// outside 0..10 is a round the original would scribble through, and this port draws nothing).
fn map_cell(ds_img: &[u8], base: usize, index: usize) -> Option<i32> {
    if index > 0x14 || base + index + 1 >= ds_img.len() {
        return None;
    }
    Some(u16le(ds_img, base + index) as i32)
}

/// Draw the marker record at `rec` with `bottom = row`, `x = col` bytes — `CS:3455` with the
/// strip's own pitch (`ax = 0x2C`), no scroll (`DS:0x31B2 = 0`) and no mirror (`dx = 0`).
fn map_blit(strip: &mut [u8; STRIP_LEN], ds_img: &[u8], rec: u16, col: i32, bottom: i32) {
    let at = rec as usize;
    if at >= ds_img.len() {
        return;
    }
    if let Some(r) = Record::expand_planes(&ds_img[at..]) {
        masked_blit(strip, &r, col, bottom, false);
    }
}

/// One dot of the map's part layer, exactly as `CS:2C60` computes it.
///
/// `row` is the blit's `BX` (the lattice row **minus two**), `col` its `CX` (the lattice column
/// **plus one**), so the sprite's own three rows start three rows above `row` — the offset the
/// recordings show (stage 19b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapDot {
    /// `DS:0x3219 + slot` — the record's screen, kept so a probe can say which part a dot is.
    pub screen: i16,
    /// The record slot, `2`/`4`/`6`/`8` (= carried code 9/10/11/12).
    pub slot: usize,
    pub row: i32,
    pub col: i32,
}

/// **`CS:2C60`** — the dots, one per position record, and the whole of their visibility rule.
///
/// ```text
/// 2c60  push si ; di = 8
/// 2c64  cmpw $0x0,0x3219(%di)        ; the record's screen
/// 2c69  jl   0x2c9d                  ; < 0 -> no dot. NOTHING ELSE IS CONSULTED
/// 2c6b  bx = word[0x3219+di] ; bx = word[0x15d8+bx] ; bx -= 2
/// 2c76  k  = byte[0x320f+di] / 0x1a  ; 156 entries a row / 26 = six map columns
/// 2c85  cx = word[0x15e2 + 2*k] ; inc cx
/// 2c90  blit(0x148c) at (bx, cx)
/// 2c9d  di -= 2 ; jne 0x2c64         ; slots 8, 6, 4, 2 = carried codes 12, 11, 10, 9
/// ```
///
/// **There is no "found" gate on these dots, and there cannot be one.** The loop reads five words
/// — `0x3219+di`, `0x320F+di` and the two lattice tables — and returns. It never reads the kit word
/// (`0x32A3`/`0x32A5`), the deposited-parts mask (`0x2EB8`), the hands (`0x2F2C`/`0x2F2E`) or the
/// per-part owner (`0x322D`). The records are written for carried codes 9..12 by the *round-start*
/// placement (`CS:0463 → CS:257B`, once per round from `CS:060D`), cleared to `0xFFFF` when the
/// object is picked up (`CS:383F`/`CS:387F`) and re-recorded when it is put down (`CS:39B0`) or the
/// case is deposited (`CS:2327`) — so a dot means **"this object is on a floor somewhere"**, and
/// all four are on the map from the round's first frame. Stage 19b measured this against the
/// recordings: on the **103** map frames whose framebuffer is usable, the dot set the recording
/// shows is exactly the record-derived set (dot counts 1/2/3/4 all occur), including the
/// earliest-round frames, where no part had been found by either spy.
pub fn map_dots(ds_img: &[u8]) -> Vec<MapDot> {
    let mut out = Vec::with_capacity(4);
    if ds_img.len() < 0x10000 {
        return out;
    }
    // `CS:2C61` — `di = 8` and down, so slot 8 (code 12) is drawn first and slot 2 (code 9) last.
    for di in [8usize, 6, 4, 2] {
        let screen = u16le(ds_img, DS_POS_RECORD + di) as i16;
        if screen < 0 {
            continue;
        }
        let k = (ds_img[DS_POS_COL + di] / MAP_COL_STRIDE) as usize;
        // `CS:2C6B`/`CS:2C89`: the dot's row is the lattice row **minus two** and its column the
        // lattice column **plus one** — the dot sits above the intersection, not on it.
        let (Some(row), Some(col)) = (
            map_cell(ds_img, MAP_ROWS_TAB, screen as usize),
            map_cell(ds_img, MAP_COLS_TAB, 2 * k),
        ) else {
            continue;
        };
        out.push(MapDot { screen, slot: di, row: row - 2, col: col + 1 });
    }
    out
}

/// **The map window** (`CS:2B7F`), drawn into one console's strip.
///
/// Returns what it wrote, for the probes and the report: `(plate, legs, parts, own)` — the plate
/// is 1 when the window was painted at all, so a `(0, ..)` result means the spy has no map up or
/// the routine's own hide rule fired.
pub fn draw_map(strip: &mut [u8; STRIP_LEN], ds_img: &[u8], player: usize) -> (usize, usize, usize, usize) {
    let o = 2 * player;
    if ds_img.len() < 0x10000 {
        return (0, 0, 0, 0);
    }
    // `CS:2BAE` — the plate. 45 rows of 22 bytes, `rep movsb` per row into the strip.
    for row in 0..MAP_PLATE_ROWS {
        let src = DS_MAP_PLATE + 2 + row * MAP_PLATE_BYTES;
        if src + MAP_PLATE_BYTES > ds_img.len() {
            return (0, 0, 0, 0);
        }
        let dst = (MAP_ROW + row) * STRIP_PITCH + MAP_COL;
        strip[dst..dst + MAP_PLATE_BYTES]
            .copy_from_slice(&ds_img[src..src + MAP_PLATE_BYTES]);
    }

    // `CS:2BC4` — the four legs of the trail. Each pair of consecutive entries is one leg; the
    // loop draws a marker at *both* ends, choosing the record by the leg's length and the pair
    // order the sign of the difference decides (`0x147C` takes the far end, `0x147E` the near one).
    let mut legs = 0usize;
    let bp = u16le(ds_img, DS_MAP_TRAIL_PTR + o) as usize;
    if bp + 10 <= ds_img.len() {
        for di in [6usize, 4, 2, 0] {
            let a = u16le(ds_img, bp + di);
            let b = u16le(ds_img, bp + di + 2);
            let diff = b.wrapping_sub(a) as i16;
            if diff == 0 {
                continue;
            }
            let span = diff.unsigned_abs();
            let (hi, lo) = if diff < 0 { (a, b) } else { (b, a) };
            let pair = if span <= MAP_LEG_SHORT_MAX {
                MAP_REC_LEG_SHORT
            } else {
                MAP_REC_LEG_LONG
            };
            // `si = 2` first (the `0x147E` end, record from the pair's second slot), then `si = 0`.
            for (tgt, rec) in [(lo, pair[1]), (hi, pair[0])] {
                let q = (tgt / 0x18) as usize;
                let r = (tgt % 0x18) as usize;
                let (Some(row), Some(col)) = (
                    map_cell(ds_img, MAP_ROWS_TAB, 2 * q),
                    map_cell(ds_img, MAP_COLS_TAB, r),
                ) else {
                    continue;
                };
                map_blit(strip, ds_img, rec, col, row);
                legs += 1;
            }
        }
    }

    // `CS:2C60` — one dot per valid position record (carried codes 9, 10, 11, 12). The rule is the
    // record's own sign and nothing else: see [`map_dots`], which is this loop.
    let mut parts = 0usize;
    for d in map_dots(ds_img) {
        map_blit(strip, ds_img, MAP_REC_PART, d.col, d.row);
        parts += 1;
    }

    // `CS:2CA3` — the blinking "you are here". Skipped while the display word is negative, which
    // is the half of the blink the recorded strips show as an empty cell.
    let mut own = 0usize;
    if (u16le(ds_img, DS_MAP_FLAG + o) as i16) >= 0 {
        let band = u16le(ds_img, DS_BAND + o);
        let b = if band >= 10 { band - 10 } else { band };
        let colidx = u16le(ds_img, DS_MAP_COL + o) as usize;
        let (Some(row), Some(col)) = (
            map_cell(ds_img, MAP_ROWS_TAB, b as usize),
            map_cell(ds_img, MAP_COLS_TAB, colidx),
        ) else {
            return (1, legs, parts, 0);
        };
        map_blit(strip, ds_img, MAP_REC_OWN, col, row);
        own = 1;
    }
    (1, legs, parts, own)
}

// ---- the per-cell object layer (FUN_26c0_0be7) --------------------------
/// Three rows of 24 entries, row pitch 0x9C, window at `block + (mapX>>1) + 0x99`.
/// Each entry is one byte, one entry = 8 px of world width. Codes 1..0x13 are objects; a code with
/// bit 7 set takes the shared record at `DS:0x6F80`.
///
/// **Where an object lands.** `Y` is the *wrap count* of `di - block` folded by 0x9C, not the row
/// number: the walk already adds 0x99 (one row minus 3), so every row wraps at least once and the
/// three windows take `Y = table[2], table[4], table[6]`. But Y is `table[0]` (34) for a window that
/// wraps zero times. Pairing Y with the row index instead puts every object 8 rows too high while
/// leaving the column right, which is what made stage 1's objects look like a decode fault.
///
/// The objects stay off whenever `DS:0x2F7E` is set for this player -- the routine returns
/// immediately at its first instruction in that case.
// ---- the round-start object layout (FUN_26c0_257b, find-a-spot 0x272e) ----
/// `DS:0x0002` — the island's object count, loaded by the round-start island copy from the island
/// record's `+0x0E` field (`IslandData::objects_unknown`). The placement steps it down by 2, so it
/// places `count / 2 + 1` objects: 14, 20, 30, 34, 34 and 34 for islands 1..7 (26/38/58/66/66/66).
pub const DS_OBJ_COUNT: usize = 0x0002;
/// `DS:0x3327` — one word per object slot: the code to place (low byte).
pub const DS_OBJ_CODES: usize = 0x3327;
/// `DS:0x336B` — one word per slot: 1 means "hidden under a scenery mound", i.e. OR 0x80 into the
/// code. Slot 0 is the only *visible* dynamite on a 14-object island; the rocket parts, the
/// briefcase, the water bucket, both plungers, snowshoes, the saw and the ice pick are all buried.
pub const DS_OBJ_BURIED: usize = 0x336B;
/// `DS:0x31B6` — word: the address of the rocket hatch's middle entry (written by the placement and
/// read by the round loop when it writes `0x0F`/`0x15`).
pub const DS_OBJ_HATCH_PTR: usize = 0x31B6;
/// Entries per object row: 156, one entry per 8 px of world width (13 per 26-px cell).
pub const OBJ_ROW_LEN: usize = 0x9C;
/// Bytes per object block: **four** rows of 156, i.e. `0x270`. Only rows 1..3 take objects.
pub const OBJ_BLOCK_LEN: usize = 0x270;
/// The first row the placement writes (`off = 0x9C + 2k`), so y = 40, 48 and 56 and never 34.
pub const OBJ_FIRST_PLACED_ROW: usize = 1;
/// The hatch cover, closed, and the inert flanking byte the round loop puts either side of it
/// (`0x15` is outside the drawn code range, so the flanks are markers, not sprites).
pub const HATCH_CLOSED: u8 = 0x0F;
pub const HATCH_FLANK: u8 = 0x15;
/// `0x10` — the hatch **open**: written by `FUN_26c0_2392` (`CS:241b`) when a spy's own cell holds
/// a hatch-family byte, and the code a spy may stand in without the pickup routine taking it.
pub const HATCH_OPEN: u8 = 0x10;
/// `0x11` — the **polar bear**. `FUN_26c0_2392` (`CS:23b7`) writes it into the entry *two to the
/// right of the hatch's middle* after the descending spy's animation state `0x20` has run ten
/// frames; the object layer draws it with `x - 5` (`CS:0c6d`), which is why it reads as sitting on
/// the hatch. The byte the spawn overwrote is saved in `DS:0x31B8` and restored when the hatch
/// closes again.
pub const BEAR: u8 = 0x11;

/// `DS:0x31D5` — the hatch's armed word. **0 = the launch site is on HIDE: the hatch is not in the
/// object block at all** (the placement's write is skipped, `CS:2696`) and only appears when the
/// deposited mask has all three parts (`CS:0610`: `if (0x31D5 == 0 && 0x2EB8 == 0x6C0)` → write
/// `15 0F 15`, `0x31D5 = 2`). 2 = SHOW, written at placement, or set by that reveal.
pub const DS_HATCH_ARMED: usize = 0x31D5;
/// `DS:0x31B8` — the byte the bear's spawn saves out of the hatch's `+2` entry.
pub const DS_HATCH_SAVE: usize = 0x31B8;
/// `DS:0x2FB0` — how many of the island's igloo interiors carry a fire this round.
pub const DS_FIRES: usize = 0x2FB0;

// ---- stage 15: the ascent (`FUN_26c0_20ee`, `CS:2156`, `CS:21ae`) -----------------------
/// `DS:0x31BF` — the **difficulty** word: the options screen's difficulty zone steps it 0, 2, .., 8
/// (`CS:1066`), i.e. 0 for difficulty 1 and 8 for difficulty 5. It *is* the round's own word: the
/// round start copies it nowhere. Measured on `rocket-room-4-hard`, where the owner switched from
/// difficulty 1 to 5 mid-session: 0 -> 8 at t 456.4 s.
pub const DS_DIFF: usize = 0x31BF;
/// `DS:0x61FA` — the **rocket** record: `[160 rows][9 bytes]`, then the AND and OR planes.
pub const DS_ROCKET: usize = 0x61FA;
/// `DS:0x2D34`/`0x2D36` — the two **flame** records, chosen by `0x2EBC & 2` (`CS:2197`).
pub const DS_FLAME: usize = 0x2D34;
/// The blitter's `BX` for both the rocket and the flame: `0x36` (`CS:20ff`, `CS:215e`).
pub const ASCENT_BOTTOM: i32 = 0x36;
/// `0x9E` — the rocket's shift base: `0x31B2 = 0x9E - 0x2EBC` (`CS:2121`).
pub const ROCKET_SHIFT_BASE: i32 = 0x9E;
/// `0xBA` — the flame's (`CS:2180`).
pub const FLAME_SHIFT_BASE: i32 = 0xBA;
/// `0xD2` — past this the rocket's body is not drawn at all (`CS:20f7`).
pub const ASCENT_HIDE: u16 = 0xD2;
/// `0x96` — from this the flame is drawn (`CS:2148`).
pub const ASCENT_FLAME: u16 = 0x96;
/// `0xEC` — and past this it is not (`CS:215c`).
pub const ASCENT_FLAME_END: u16 = 0xEC;
/// `0xEE` — the last counter value: `0x248A = 1`, the round-over flag (`CS:21ae`).
pub const ASCENT_END: u16 = 0xEE;

/// The ascent's placement, exactly as `CS:2121`/`CS:212a` compute it: `shift = base - counter`, and
/// when that goes negative the sprite stops sliding and its **bottom** rises instead.
pub fn ascent_place(asc: u16, base: i32) -> (i32, i32) {
    let mut bottom = ASCENT_BOTTOM;
    let mut shift = base - asc as i32;
    if shift < 0 {
        bottom += shift;
        shift = 0;
    }
    (bottom, shift)
}

/// The rocket's record, as the blitter wants it. `DS:0x61FA` is passed straight to `0x3455` (no
/// expansion-table detour), so it is the two-plane form. Its own header says **160 rows x 9
/// bytes**, which is why this does not go through [`Record::expand_planes`]'s 64-row guard.
pub fn rocket_record(ds: &[u8]) -> Option<Record<'static>> {
    record_planes(ds.get(DS_ROCKET..)?, 256)
}

/// The flame's record for this counter value: `word[0x2D34 + (asc & 2)]` (`CS:2197`).
pub fn flame_record(ds: &[u8], asc: u16) -> Option<Record<'static>> {
    let off = ds.get(DS_FLAME..DS_FLAME + 4)?;
    let ptr = u16::from_le_bytes([off[(asc & 2) as usize], off[(asc & 2) as usize + 1]]) as usize;
    record_planes(ds.get(ptr..)?, 256)
}

/// `[h][w]` then an AND plane and an OR plane of `h*w` bytes each — the rocket's and the flame's
/// form. `max_rows` is the caller's own sanity bound (160 and 38 here; the 64 in
/// [`Record::expand_planes`] is the *sprite* tables' own limit and would reject the rocket).
pub fn record_planes(src: &[u8], max_rows: usize) -> Option<Record<'static>> {
    if src.len() < 4 {
        return None;
    }
    let (h, w) = (src[0] as usize, src[1] as usize);
    if h == 0 || w == 0 || h > max_rows || w > 32 {
        return None;
    }
    let n = h * w;
    if src.len() < 2 + 2 * n {
        return None;
    }
    Some(Record::from_planes(h, w, src[2..2 + n].to_vec(), src[2 + n..2 + 2 * n].to_vec()))
}

/// The reveal's mask value: `0x400` (the briefcase exists) plus the three parts' bits `0x40`,
/// `0x80`, `0x200`. `CS:0617` compares the whole word.
pub const MASK_HATCH_REVEAL: u16 = 0x6C0;
/// The kit bits of the three rocket parts — `CS:284d`'s gate on the *per-spy* kit word (not the
/// shared mask): all three found and the hatch launches the rocket, otherwise the bear comes out.
pub const KIT_ALL_PARTS: u16 = 0x2C0;

// ---- the igloo interiors' fire (CS:26ae..0x26f3) -------------------------
/// Level 0x12 (18): the igloo-interior dome plate **without** a fire (`plate[0x12] = 0x3ae8`).
pub const LEVEL_INTERIOR_BARE: u16 = 0x12;
/// Level 0x16 (22): the dome plate **with** a flame (`plate[0x16] = 0x3504`). The round-start
/// lottery turns a bare interior into this one with probability 0xA1/0x100.
pub const LEVEL_INTERIOR_FIRE: u16 = 0x16;
/// Level 0x18 (24): the igloo dome as drawn in the *outdoor* room (`plate[0x18] = 0x4bd0`), and
/// what the lottery makes of one cell in four whose level is 2.
pub const LEVEL_IGLOO_DOOR: u16 = 0x18;
/// Level 2 — the plain igloo-door candidate the lottery rolls against.
pub const LEVEL_IGLOO_PLAIN: u16 = 2;

/// The animation states the hatch sequence runs (`DS:0x32CF` / `DS:0x32CB`).
pub const STATE_DESCEND: u16 = 0x1E;
pub const STATE_BEAR: u16 = 0x20;
pub const STATE_CLOSE: u16 = 0x1C;

/// **State 0 — the item take** (`CS:38bf`, stage 25a). Every successful take, deposit or lift of a
/// spy's own stick writes `0x32CB = 0` (`CS:3859`, `CS:37ba`), loads the item's own pose set into
/// `0x2EF8` and calls `FUN_26c0_277c`, so the take runs as a two-frame animation: pose set
/// `word[0x2CC4 + 0] = 0x84` (132), ceiling `word[0x2CE6 + 0] = 2`. It is the only state below
/// `0x1E` the *body* starts on itself rather than on the other spy, and it is what holds the base
/// counter `0x2F54` for three frames in the demo (the recording's heat is exact against it).
pub const STATE_TAKE: u16 = 0x00;

// ---- stage 13a: the tool animations and the hazard animations -------------------------------
//
// All of these are `FUN_26c0_277c` states: the state word goes into `0x32CF`, the pose set comes
// from `word[0x2CC4 + state]`, the frame word is seeded with -2 and the animation's ceiling is
// `word[0x2CE6 + state]` (end when `frame + 2 > ceiling`, i.e. `ceiling/2 + 1` drawn frames).
// `FUN_26c0_2de3` starts 4/6/8, and `FUN_26c0_2444` starts 0x0C/0x0E/0x10/0x1A.
pub const STATE_SAW: u16 = 0x04;
pub const STATE_ICE_PICK: u16 = 0x06;
pub const STATE_BUCKET: u16 = 0x08;
/// The detonator's own animation (`CS:2e48`). Started in stage 13b with the plunger.
pub const STATE_PLUNGER: u16 = 0x0A;
/// The laugh: the *other* spy's reaction to any of the three hazards (`CS:2480`, `0x24de`, `0x252f`).
pub const STATE_LAUGH: u16 = 0x0C;
/// Slipping on spilled water (`CS:248a`).
pub const STATE_SLIP: u16 = 0x0E;
/// Falling into the saw's hole (`CS:24ba`) — and the ice-terrain slip (`CS:1e64`) and the door
/// animation share it.
pub const STATE_FALL: u16 = 0x10;
/// The explosion's pose (`CS:2e27`, the plunger's victim) — stage 13b.
pub const STATE_BLAST: u16 = 0x12;
/// **Freezing to death** (`CS:228e`, stage 14): pose set `word[0x2CC4 + 0x14] = 0x228`, ceiling
/// `word[0x2CE6 + 0x14] = 110` -> 56 drawn frames. Its end is the only one that takes the spy out
/// of the round (`CS:294d`).
pub const STATE_FREEZE: u16 = 0x14;
/// Knocked out by an icicle (`CS:2539`).
pub const STATE_KNOCKOUT: u16 = 0x1A;

// ---- stage 16b: the snowball ---------------------------------------------------------------
/// The throw (`CS:302c`): pose set `0x88`, ceiling 14 -> **eight** drawn frames. The ball leaves on
/// frame 8 (`CS:09b1`), which is the fifth drawn frame of the eight. It is *not* in the table's
/// 0x1E-and-up range, so `277c` also zeroes `0x2F0C` — the direction word the throw itself writes
/// (0 east, 6 west) never survives the call, which is why a recorded throw always shows dir 0 and
/// only the facing (`0x2F14`) tells the two apart.
pub const STATE_THROW: u16 = 0x02;
/// The snowball's two knock-outs (`CS:30f4..3126`), chosen by the victim's facing plus the ball's
/// velocity: `0x2F14 + 0x32FB` = 4 or -3 gives **0x16** (the ball meets his face), 3 or -2 gives
/// **0x18** (it takes him from behind). Pose sets `0x2AC`/`0x2B0`, ceiling 2 -> **two** frames:
/// a hit reaction, not the icicle's 36-frame knock-out.
pub const STATE_KNOCK_FACE: u16 = 0x16;
pub const STATE_KNOCK_BACK: u16 = 0x18;
/// `DS:0x6F66` — the thrown ball's sprite: `[height][width_bytes]` then a mask plane and a data
/// plane, the object-layer format. 6 rows x 2 bytes (8 pixels) wide, bottom row on `0x32EF`.
pub const BALL_RECORD: usize = 0x6F66;

/// `0x2EFC`'s seed value when an animation state starts (`CS:277c`): -2. It is *never drawn* — the
/// same frame's tail adds `0x2F08` before the sprite — but it is what the port's state machine
/// steps from, so the pose tables are fed the advanced value (see [`PoseParams::records`]).
pub const SEED_FRAME: u16 = 0xFFFE;
/// `DS:0x2F74` reaching ten is what spawns the bear (`CS:23a4`).
pub const BEAR_TRIGGER_FRAMES: u16 = 10;
/// The PRNG's fixed `DX` on the attract-mode branch: `dh` = seconds, `dl` = hundredths.
pub const RNG_FIXED_DH: u8 = 0x77;
pub const RNG_FIXED_DL: u8 = 0x0D;
/// `DS:0x3203` — non-zero selects the fixed `DX` above instead of the DOS clock, and `DS:0x31B0`
/// is seeded with `0x469`. That is the attract round, and it is the only round whose layout can be
/// replayed: in normal play every draw takes `dh`/`dl` from `int 21h ah=2Ch`, so the layout depends
/// on the wall clock at the moment of the round start and is not recoverable from a dump.
pub const RNG_ATTRACT_SEED: u16 = 0x0469;

/// **Stage 24a** — the attract demo's recorded input stream: `DEMO.DAT`, 1,882 bytes = 941 frames
/// of one 16-bit key mask each.
///
/// `CS:0203` is the whole of `CS:0325`'s attract branch and it is 194 bytes long: clear all twelve
/// key words, then
///
/// ```text
///  263  bx = word[0x0FB6]                 ; a byte offset, 0 at the round start (`CS:0F44`)
///  267  ax = word[bx + 0x0D]              ; this frame's mask
///  26b  bit 0 -> 0x3287 (P1 up)   1 -> 0x3293 (P1 right)   2 -> 0x328B (P1 down)
///       bit 3 -> 0x328F (P1 left) 4 -> 0x327F (P1 A)       5 -> 0x3283 (P1 B)
///       bits 8..13 -> the same six for P2 (0x3289/0x3295/0x328D/0x3291/0x3281/0x3285)
///  31f  word[0x0FB6] += 2                 ; one mask a frame, for ever: nothing ever rewinds it
/// ```
///
/// `DS:0x000D` is exactly where the loader puts **`DEMO.DAT`** — the file sits in the original's
/// disk directory beside `spy.exe`, 1,882 bytes, and a recorded demo dump's `DS:0x000D..` is that
/// file byte for byte (`m1-fix-960` ds_00003 and `playtest3` ds_00246 agree). The thirteen bytes
/// before it are the loader's own header (`0x01FC`, `0x001A`, then a zero fill).
///
/// Neither the keyboard nor the computer reaches a demo frame (`CS:0325` returns before `0x332` and
/// before the `CS:129C` call), so both spies are driven by this table alone — which is what the
/// owner sees as "the game auto-playing itself with both spies automated".
pub const DEMO_FRAME_BYTES: usize = 2;
/// The mask's two six-bit fields: bits 0..5 spy 1, bits 8..13 spy 2 (`CS:0203`'s twelve tests).
pub const DEMO_MASK_BITS: u16 = 0x3F3F;

/// Parse a `DEMO.DAT` image into its per-frame key masks (see [`DEMO_FRAME_BYTES`]).
///
/// Odd trailing bytes are dropped, so a truncated file simply yields fewer frames; past the end the
/// game reads whatever the load left in the data segment (`DS:0x000D + 1882` onward reads zero in
/// both recorded demos) and never rewinds, which the engine reproduces by returning mask 0.
pub fn demo_masks(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(DEMO_FRAME_BYTES)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// `DS:0x329F` gives the object block index as `band / 2`, and the placement only ever draws an
/// even band 0..8, so five blocks cover the five outdoor rows. Rows 5..9 (the igloo interiors) have
/// no object grid at all.
pub const OBJ_BANDS: [u16; 5] = [0, 2, 4, 6, 8];

/// The original's PRNG, `FUN_26c0_20cd` (CS:0x20CD):
///
/// ```text
///   state = ((state_lo + dh) * dl) & 0xFFFF        ; an 8x8 multiply
///   return state                                   ; callers use the low byte
/// ```
///
/// Only the low byte of the state feeds the next draw, so the output sequence is the 256-cycle
/// `o -> ((o + dh) * dl) & 0xFF` and the high byte is irrelevant. `DX` is **not** a constant in the
/// original: the routine issues `INT 21h AH=2Ch` (`DH` = seconds, `DL` = hundredths) on **every**
/// call, unless the attract-mode flag `DS:0x3203` is set, in which case `DX` is the fixed `0x770D`.
/// So a normal round's layout is a function of the wall clock at the moment of the round start, and
/// cannot be recovered from a dump — see [`Self::from_clock`] and [`CLOCK_TICK_DRAWS`].
///
/// Frozen-`DX` mode (the default, and what [`Self::at`]/[`Self::from_seed`] give) is the port's
/// deterministic path: with `dh`/`dl` pinned the whole reachable set is the 256 rotations of one
/// cycle, which is what every test, probe and recorded replay depends on.
#[derive(Clone, Debug)]
pub struct ObjRng {
    state: u16,
    dh: u8,
    dl: u8,
    /// `true` = `DX` is the DOS clock's (the original's normal-play branch): [`Self::from_clock`]
    /// took a reading at the round start and advances it as the round's draws go by.
    clock: bool,
    /// Draws spent in the current hundredth (clock mode only, see [`CLOCK_TICK_DRAWS`]).
    clock_phase: u8,
    pub draws: u32,
}

/// How many times [`dos_clock_dx`] has been called, process-wide.
///
/// The engine never reads this; it exists so a test can show that a clock-seeded round takes its
/// **own** reading (and that a pinned round takes none at all).
pub static CLOCK_READS: AtomicU32 = AtomicU32::new(0);

/// `INT 21h AH=2Ch`'s `DX`: `DH` = seconds (0..59), `DL` = hundredths (0..99).
///
/// The original reads the *local* DOS clock; UTC vs local only rotates the phase of an already
/// uniform draw, so the port reads `SystemTime` and takes both fields off `UNIX_EPOCH`. Nothing
/// depends on the absolute value, only on it differing between rounds.
pub fn dos_clock_dx() -> (u8, u8) {
    CLOCK_READS.fetch_add(1, Ordering::Relaxed);
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let hundredths = (d.subsec_nanos() / 10_000_000) as u8; // 0..99
    ((secs % 60) as u8, hundredths.min(99))
}

/// **How long one hundredth of the DOS clock lasts, in the port's draws.**
///
/// The clock itself only ticks ~18 times a second (`INT 21h AH=2Ch` reads the BIOS tick counter), so
/// a hundredth is 5.5 ms — but an 8086 spends tens of microseconds *inside* `INT 21h` itself, and
/// `CS:257b` makes ~260 of those calls (measured: 261 draws to place island 1, STAGE22B §2.2). One
/// original placement therefore spans **two or three** hundredths, which is the only reason the two
/// unbounded re-roll loops in the routine (`CS:26f3`'s fire lottery and the hatch's cell search) ever
/// terminate: a `DX` whose 8-bit state cycle lands no valid draw is escaped by the next hundredth.
///
/// The port cannot copy the 8086's timings, and *reading `SystemTime` on every draw* would hold `DX`
/// fixed for the whole placement — a case no original round ever had, and one the port then hits
/// head-on: the hatch loop's own cap fires before a hundredth passes and the round comes out with
/// **no hatch at all** (1 of 40 launches measured). So the port models the clock the way the
/// original *saw* it: one reading at the round start ([`Self::from_clock`]), then one hundredth every
/// `CLOCK_TICK_DRAWS` draws. 96 draws is the measured 261-draw placement spanning ~2.7 hundredths.
pub const CLOCK_TICK_DRAWS: u8 = 96;
/// A DOS tick is 5.49 hundredths; the modelled clock steps that many per tick.
const CLOCK_TICK_STEP: u8 = 5;

impl ObjRng {
    /// The documented attract-mode seed and its fixed `DX`.
    pub fn attract() -> ObjRng {
        ObjRng::at(RNG_ATTRACT_SEED)
    }

    pub fn at(state: u16) -> ObjRng {
        ObjRng {
            state,
            dh: RNG_FIXED_DH,
            dl: RNG_FIXED_DL,
            clock: false,
            clock_phase: 0,
            draws: 0,
        }
    }

    /// A seed -> a state. The mixer is the usual 32-bit scramble folded to 16 bits; the low bit is
    /// forced so the orbit is a single 256-cycle (`dl` is odd, so `o -> (o + dh) * dl` is a
    /// permutation of the 256 bytes).
    pub fn from_seed(seed: u32) -> ObjRng {
        let x = seed.wrapping_add(0x9E37_79B9).wrapping_mul(0x85EB_CA6B);
        ObjRng::at(((x >> 16) as u16 ^ x as u16) | 1)
    }

    /// **The original's normal-play seeding**: `DX` is the DOS clock's, not the attract round's fixed
    /// `0x770D` (`CS:20CD`'s non-attract branch). A fresh generator is made for every round — the
    /// port's `Game::new` per GO! and per `r`, the original's `CS:060D` re-entering `0x440` — so every
    /// round's items land somewhere else.
    ///
    /// `dx` is a stand-in for the clock reading — what a test or a probe injects instead of the wall
    /// clock — so two rounds can be made to differ deterministically. `None` reads the real clock
    /// (once: see [`CLOCK_TICK_DRAWS`] for how the reading then advances).
    pub fn from_clock(dx: Option<(u8, u8)>) -> ObjRng {
        let (dh, dl) = dx.unwrap_or_else(dos_clock_dx);
        let mut r = ObjRng::at(((dh as u16) << 8) | dl as u16);
        r.dh = dh;
        r.dl = dl;
        r.clock = true;
        r.clock_phase = 0;
        r
    }

    /// Is this generator taking `DX` from the clock?
    pub fn is_clock(&self) -> bool {
        self.clock
    }

    pub fn byte(&mut self) -> u8 {
        let (dh, dl) = if self.clock {
            let now = (self.dh, self.dl);
            // the modelled clock: hold the hundredth for a while, then tick (`CLOCK_TICK_DRAWS`)
            self.clock_phase += 1;
            if self.clock_phase >= CLOCK_TICK_DRAWS {
                self.clock_phase = 0;
                let h = self.dl as u16 + CLOCK_TICK_STEP as u16;
                self.dl = (h % 100) as u8;
                if h >= 100 {
                    self.dh = (self.dh + 1) % 60;
                }
            }
            now
        } else {
            (self.dh, self.dl)
        };
        let lo = (self.state as u8).wrapping_add(dh);
        self.state = lo as u16 * dl as u16;
        self.draws += 1;
        self.state as u8
    }

    pub fn state(&self) -> u16 {
        self.state
    }
}

/// One placed object: which block (`band / 2`), which row of it, which entry, which code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub band: u16,
    pub row: usize,
    pub entry: usize,
    pub code: u8,
}

impl Placement {
    /// The entry's world x, in the **byte** coordinate the rest of the model uses (the gates, the
    /// spy's `x`, `map_x`): one entry is 2 bytes of world x, i.e. 8 px, because a world byte is
    /// 4 px (a cell is 26 bytes = 104 px). This is exactly the `x` the placement hands to
    /// `FUN_26c0_366e`.
    pub fn world_x(&self) -> i32 {
        2 * self.entry as i32
    }

    /// The sprite's bottom row, from `DS:0x2612` = 34, 40, 48, 56.
    pub fn bottom_row(&self) -> i32 {
        [34, 40, 48, 56][self.row.min(3)]
    }
}

/// What one round-start placement produced.
#[derive(Clone, Debug, Default)]
pub struct ObjectLayout {
    pub objects: Vec<Placement>,
    pub hatch: Option<Placement>,
}

/// The five block addresses, read out of the image's pointer table (`DS:0x2600`).
pub fn block_addr(ds_img: &[u8], band: u16) -> Option<usize> {
    let i = (band / 2) as usize;
    if i >= 5 {
        return None;
    }
    let a = u16le(ds_img, DS_BLOCK_PTR + 2 * i) as usize;
    if a + OBJ_BLOCK_LEN <= ds_img.len() {
        Some(a)
    } else {
        None
    }
}

/// `FUN_26c0_272e` — a free, terrain-clean slot in `band`.
///
/// ```text
///   do { do { r = rand(); off = 0x9c + 2*((r*10)/11) } while block[off] != 0
///        t = mask_byte(band, x = 2*(off % 0x9c), y = 8*(off / 0x9c) + 0x20)
///      } while t not in (0, 1)
///   t == 1 -> the whole band is refused (the caller draws a new band); t == 0 -> accept
/// ```
///
/// `None` = the band is refused. The band refusal is what keeps objects off rows whose cells are
/// all wall: on island 1 bands 0, 2 and 8 are mask code 1 over every candidate spot, so no object
/// ever lands on them, and the recorded layouts show exactly that (blocks 0/1/4 empty).
fn find_object_spot(
    ds: &[u8],
    ds_img: &[u8],
    levels: &[[u16; 12]; 10],
    terrain: &Terrain,
    band: u16,
    rng: &mut ObjRng,
) -> Option<usize> {
    let base = block_addr(ds_img, band)?;
    for _ in 0..4096 {
        let r = rng.byte() as usize;
        let off = OBJ_ROW_LEN + 2 * ((r * 10) / 11);
        if off + 1 >= OBJ_BLOCK_LEN {
            continue;
        }
        if ds[base + off] != 0 {
            continue;
        }
        let row = off / OBJ_ROW_LEN;
        let entry = off % OBJ_ROW_LEN;
        match terrain.code(levels, band, (entry * 2) as i32, (8 * row + 0x20) as i32) {
            Some(1) => return None,
            Some(0) => return Some(off),
            _ => continue,
        }
    }
    None
}

/// `FUN_26c0_257b`, the object half: place island `count / 2 + 1` objects at random free slots.
///
/// Called with the blocks already zeroed (`materialise_round_start`), on the level table **before**
/// the round-start `2 -> 0x18` coin flip — the original places the objects first and rigs the table
/// afterwards, and the flip changes which mask a cell reads.
///
/// Slots run from `count` down to 0 in steps of 2, so the *last* slot placed is slot 0 — the visible
/// dynamite — and the rocket parts (slots 1..3), the briefcase (4) and the tools go in first, at
/// random spots. Where a slot's flag word is 1 the code is OR-ed with 0x80, which the object layer
/// draws with the shared mound record: that is an item hidden under a snow pile.
pub fn place_round_objects(
    ds: &mut [u8],
    ds_img: &[u8],
    levels: &[[u16; 12]; 10],
    terrain: &Terrain,
    count: usize,
    rng: &mut ObjRng,
) -> ObjectLayout {
    let mut out = ObjectLayout::default();
    let mut i = count as i32;
    while i >= 0 {
        let slot = (i / 2) as usize;
        let (band, off) = loop {
            let band = ((rng.byte() as usize / 0x1A) & 0xFE) as u16;
            if let Some(off) = find_object_spot(ds, ds_img, levels, terrain, band, rng) {
                break (band, off);
            }
        };
        let Some(base) = block_addr(ds_img, band) else {
            i -= 2;
            continue;
        };
        let mut code = u16le(ds_img, DS_OBJ_CODES + 2 * slot) as u8;
        if u16le(ds_img, DS_OBJ_BURIED + 2 * slot) == 1 {
            code |= 0x80;
        }
        ds[base + off] = code;
        out.objects.push(Placement {
            band,
            row: off / OBJ_ROW_LEN,
            entry: off % OBJ_ROW_LEN,
            code,
        });
        i -= 2;
    }
    out
}

/// The rocket hatch (the same routine, after the spies): a random cell of the island whose level is
/// **0** (open ground), at row 3, with three consecutive free entries. `DS:0x31B6` then points at
/// the middle one.
///
/// **Whether the three bytes are written depends on the launch-site option.** `CS:2696`:
///
/// ```text
/// 2693  DS:0x31B6 = di                       ; the pointer is stored either way
/// 2696  if (word[0x31D5] == 0) goto 26ae     ; HIDE: no bytes, the hatch is INVISIBLE
/// 269d  [di] = 0x0F; [di+1] = 0x15; [di-1] = 0x15
/// ```
///
/// The draw loop itself runs either way, so `armed` does not change what the PRNG consumes (that
/// matters for the attract round's exact replay) — only whether the cell is inked.
///
/// Measured in the attract round: the hatch lands at band 4, row 3, entry 57 — `(gate[4] + 10) / 2`
/// = (104 + 10) / 2 — and the three bytes are `15 0F 15`. Over the corpus, the 11,456 sampled
/// dumps with `0x31D5 == 0` hold `00 00 00` at the hatch's three entries and the 7,578 with
/// `0x31D5 == 2` hold `15 0F 15` (or `15 10 15` while it is open): the option decides whether the
/// hatch exists at all, it is not a drawing switch.
pub fn place_hatch(
    ds: &mut [u8],
    ds_img: &[u8],
    levels: &[[u16; 12]; 10],
    terrain: &Terrain,
    rng: &mut ObjRng,
    armed: bool,
) -> Option<Placement> {
    let base_row = OBJ_ROW_LEN * 3;
    for _ in 0..4096 {
        let r = (rng.byte() & 0x7E) as usize;
        let row = r / 0x18;
        let cell = (r % 0x18) / 2;
        if row >= 10 || levels[row][cell] != 0 {
            continue;
        }
        let band = (row * 2) as u16;
        let entry = ((terrain.gate(cell) + 10) >> 1) as usize;
        let Some(base) = block_addr(ds_img, band) else {
            continue;
        };
        if entry == 0 || entry + 1 >= OBJ_ROW_LEN {
            continue;
        }
        let at = base + base_row;
        if ds[at + entry - 1] != 0 || ds[at + entry] != 0 || ds[at + entry + 1] != 0 {
            continue;
        }
        if armed {
            ds[at + entry - 1] = HATCH_FLANK;
            ds[at + entry] = HATCH_CLOSED;
            ds[at + entry + 1] = HATCH_FLANK;
        }
        let p = Placement {
            band,
            row: 3,
            entry,
            code: HATCH_CLOSED,
        };
        let addr = (at + entry) as u16;
        ds[DS_OBJ_HATCH_PTR..DS_OBJ_HATCH_PTR + 2].copy_from_slice(&addr.to_le_bytes());
        ds[DS_HATCH_ARMED..DS_HATCH_ARMED + 2].copy_from_slice(&(if armed { 2u16 } else { 0 }).to_le_bytes());
        ds[DS_HATCH_SAVE] = 0;
        return Some(p);
    }
    None
}

/// `FUN_26c0_257b`'s tail (`CS:26ae..0x26f3`) — **which igloo interiors have a fire**, and the
/// `2 -> 0x18` igloo-door coin flip, in one descending pass over 60 cells:
///
/// ```text
/// 26ae  word[0x2FB0] = 0                       ; the fire count
/// 26b4  for (bx = 0x76; bx >= 0; bx -= 2) {    ; 60 cells, HIGH to LOW
/// 26ba      if (word[0x2536 + bx] == 0x12) {   ; 0x2536 = 0x24BE + 0x78: the interior rows 5..9
/// 26c1          ax = rand() & 0xFF
/// 26c7          if (ax <= 0xA0) {              ; 0xA1/0x100 = 63%
/// 26cc              word[0x2536 + bx] = 0x16   ; this interior gets a fire
/// 26d2              ++word[0x2FB0]; cx = 1
///               }
///           }
/// 26d9      if (word[0x24BE + bx] == 2) {      ; the *outdoor* cell with the same index
/// 26e0          if ((rand() & 3) == 0) word[0x24BE + bx] = 0x18
///           }
///       }
/// 26f3  if (cx == 0) goto 26ae                 ; a level with no fire at all is re-rolled
/// ```
///
/// `bx` indexes two arrays at once: `0x24BE + bx` is word `bx/2` of the 120-word level table (rows
/// 0..4 for `bx < 0x78`) and `0x2536 + bx` is word 60 + `bx/2` — i.e. **the same column five rows
/// down**, which is exactly the interior of that outdoor cell. That pairing is what makes the
/// exterior and the interior of one igloo agree.
///
/// Level `0x12` draws the bare dome plate (`plate[0x12] = 0x3ae8`), level `0x16` the same dome
/// **with a flame** (`plate[0x16] = 0x3504`) and level `0x14` the flat interior fill
/// (`plate[0x14] = 0x46b0`). So the fire is not a sprite at all: it is a plate, and the lottery is
/// what decides whether a room has one.
///
/// Measured: the attract round (`m1-fix-960/ds_00004`, `playtest3/ds_00247`) replays this loop
/// exactly — interior (7,6) `0x12 -> 0x16`, (8,3) left bare, `DS:0x2FB0 = 1` — and over the corpus
/// the count is 1..3, never 0 (the retry), with the fireless interiors being the ones the lottery
/// declined.
///
/// **The re-roll is unbounded, exactly as `CS:26f3` is** (`if (cx == 0) goto 26ae`). With a *frozen*
/// `DX` whose 8-bit state cycle never draws a value `<= 0xA0` the loop cannot end in code: the
/// original only escapes because its `INT 21h` calls take long enough for the clock to tick and the
/// next hundredth to bring a new `DL`, which is what [`CLOCK_TICK_DRAWS`] models. Both pinned paths
/// (the attract round's `0x770D` and every `--seed`) land a fire on their first sweep, so the
/// unbounded loop is only ever reached in clock mode.
pub fn place_igloo_fires(levels: &mut [[u16; 12]; 10], rng: &mut ObjRng) -> u16 {
    loop {
        let mut fires = 0u16;
        let mut got = false;
        for cell in (0..60usize).rev() {
            let (row, col) = (cell / 12, cell % 12);
            if levels[row + 5][col] == LEVEL_INTERIOR_BARE {
                if rng.byte() as u32 <= 0xA0 {
                    levels[row + 5][col] = LEVEL_INTERIOR_FIRE;
                    fires += 1;
                    got = true;
                }
            }
            if levels[row][col] == LEVEL_IGLOO_PLAIN && rng.byte() & 3 == 0 {
                levels[row][col] = LEVEL_IGLOO_DOOR;
            }
        }
        if got {
            return fires;
        }
    }
}

/// One frame of a descent/animation script — `CS:09be..0xa03`. The three words the raised branch
/// reads for animation state `state` at frame `frame`, out of the executable's own tables:
///
/// ```text
/// bp = word[0x2CC0 + 2*(state - 0x1E)]   ; the script base (0x2C00 for 0x1E, 0x2C60 for 0x20)
/// bp += 0x2EFC + 0x2F08                  ; the frame counter + its step = 2*frame
/// vy   = word[bp]; bp += count + 2
/// vx   = word[bp]; bp += count + 2
/// clip = word[bp]                        ; written to DS:0x2F68
/// ```
///
/// `0x2EFC` starts at -2 and steps by 2 (`CS:281b`: `word[0x2EFC] = 2` on the way in), so with
/// `frame` counting elapsed frames from 0 the read offset is `base + 2*frame`. `count` is
/// `word[0x2CE6 + state]`, the state's frame ceiling (30 for `0x1E` and `0x20`, 16 for `0x1C`).
/// `bp = word[0x2CC0 + (state - 0x1E)]` — a **byte** offset, exactly as `CS:09ca` computes it
/// (`bp = 0x32CF - 0x1E`, then `mov ds:0x2CC0(%bp)`), so 0x1E reads the word at 0x2CC0 (0x2C00)
/// and 0x20 the word at 0x2CC2 (0x2C60) — the table's entries are two bytes apart but the index
/// steps by one state, not by two.
pub fn descent_step(ds_img: &[u8], state: u16, frame: u16) -> Option<(i32, i32, i32)> {
    if state < STATE_DESCEND {
        return None;
    }
    let base = u16le(ds_img, 0x2CC0 + (state - STATE_DESCEND) as usize) as usize;
    let count = u16le(ds_img, 0x2CE6 + state as usize) as usize;
    let off = base.wrapping_add(2 * frame as usize) & 0xFFFF;
    let vy = u16le(ds_img, off) as i16 as i32;
    let off2 = (off + count + 2) & 0xFFFF;
    let vx = u16le(ds_img, off2) as i16 as i32;
    let off3 = (off2 + count + 2) & 0xFFFF;
    let clip = u16le(ds_img, off3) as i16 as i32;
    Some((vy, vx, clip))
}

/// The frame ceiling of an animation state (`DS:0x2CE6 + state`), which is where the state ends:
/// the code's test (`CS:0996..09a2`) is `if (0x2EFC + 0x2F08 > word[0x2CE6 + 0x32CF]) end`, and
/// `0x2EFC` is `2f - 2` on frame `f` (it starts at `-2`, `CS:27f7`), so a state plays **`ceiling/2
/// + 1`** frames: 16 for `0x1E`/`0x20` (30) and 9 for `0x1C` (16). The `+1` is the recorded
/// descent's length — 1.6 s at 10 Hz, `playtest4` ds_01774 → ds_01783.
pub fn state_ceiling(ds_img: &[u8], state: u16) -> u16 {
    u16le(ds_img, 0x2CE6 + state as usize)
}

/// `word[0x2CC4 + state]` — the pose set `FUN_26c0_277c` switches an animation state to (`CS:27e9`).
///
/// The table is read with the state word as a **byte** offset, like `0x2CC0`, so only even states
/// are meaningful; the states the hatch sequence runs (`0x1C`, `0x1E`, `0x20`) never reach this
/// read — they keep the spy's own set — and `0x0C`, the other spy's laugh, is `0xDC`.
pub fn anim_set(ds_img: &[u8], state: u16) -> u16 {
    u16le(ds_img, 0x2CC4 + state as usize)
}

pub fn draw_objects(strip: &mut [u8; STRIP_LEN], ds_img: &[u8], st: &RoundState, player: usize) -> usize {

    let p = &st.players[player];
    if p.blank || p.band > 8 {
        return 0;
    }
    let band = p.band as usize;
    let idx = band / 2;
    if idx >= st.block_ptr.len() {
        return 0;
    }
    let base = st.block_ptr[idx] as usize;
    let mut drawn = 0;
    for row in 0..3usize {
        let off = (p.map_x as usize >> 1) + 0x99 + 0x9C * row;
        for k in 0..0x18usize {
            let at = base + off + k;
            if at >= st.ds.len() {
                continue;
            }
            let code = st.ds[at];
            // `cmp byte[di],0x14 / jge` is a SIGNED compare, so 0x80..0xFF pass as negative and take
            // the shared bit-7 record; only 1..0x13 are object codes.
            if code == 0 || (code < 0x80 && code >= 0x14) {
                continue;
            }
            let mut ax = off + k;
            let mut wrap = 0usize;
            while ax >= 0x9C {
                ax -= 0x9C;
                wrap += 2;
            }
            let mut x = (ax as i32) * 2 + 3 - p.map_x as i32;
            let bottom = u16le(&st.ds, (DS_OBJ_ROW + wrap) & 0xFFFF) as i32;
            let rec_off = if code & 0x80 != 0 {
                DS_OBJ_FIXED
            } else {
                let c = (code & 0x1F) as usize;
                if c == 0x11 {
                    x -= 5; // code 0x11 (the polar bear) is shifted left 5 bytes at the call site
                }
                u16le(ds_img, DS_OBJ_TABLE + 2 * c) as usize
            };
            if rec_off >= ds_img.len() {
                continue;
            }
            if let Some(rec) = Record::expand_planes(&ds_img[rec_off..]) {
                masked_blit(strip, &rec, x, bottom, false);
                drawn += 1;
            }
        }
    }
    drawn
}

// ---- the spy layer (FUN_26c0_0c93) -------------------------------------
/// The pose parameters one spy is drawn from, as `FUN_26c0_0c93` reads them.
///
/// Two masked blits at the same x: the **body** record and the **legs overlay**. Both are selected
/// by walking two byte-offset tables, so the pose map is *derived*, not observed:
///
/// ```text
/// body = 0x7DC0 + word[0x2628 + 0x32AB + 0x2EF8 + 0x2F0C (+frame on the raised branch)]
/// legs = 0x7DC0 + word[0x2628 + word[0x2622 + 2*0x2F0C] (+0x12 if 0x2F2C==7)
///                              + (2 if raised else 0x2EFC) + 0x32AB]
/// ```
///
/// The body's bottom row is `Y-9` (the routine's own `sub $9,%bx`); on the *raised* branch
/// (`DS:0x2F18` non-zero, and `DS:0x32CF < 0x1C`) that subtraction is skipped and the body sits on
/// `Y`. The legs always sit on `Y`, and are skipped entirely when `DS:0x2F68 > 6`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoseParams {
    /// `DS:0x2F0C` — 0 horizontal, 2 up, 4 down (in/out of the screen).
    pub dir: u16,
    /// `DS:0x2EF8` — the pose set; 36 (`0x24`) is the plain walk, 108 (`0x6C`) the walk carrying
    /// an item, 132 (`0x84`) another set seen in the recordings. Selects a block of records.
    pub set: u16,
    /// `DS:0x2EFC` — 2 picks the first legs overlay, 4 the second; this is the walk animation.
    pub frame: u16,
    /// `DS:0x32AB` — extra byte offset into the record table (0 for player 1 in every dump).
    pub extra: u16,
    /// `DS:0x2F14` — facing left. Reversing the four pixels of each byte *and* the byte order of
    /// each row reproduces it; reversing the plane array instead does not.
    pub mirror: bool,
    /// `DS:0x2F2C == 7` shifts the legs row by 0x12 (not seen in the recordings).
    pub row_shift: bool,
    /// `DS:0x2F18` — the "raised" branch: climbing / entering, where the `sub $9` is skipped.
    pub raised: bool,
    /// `DS:0x32CF` — the pose's own sub-state. `0x0C` and `0x14` arm the vertical clip offset.
    pub state: u16,
    /// `DS:0x2F68` — the vertical clip/scroll offset used on the raised branch (0 in every
    /// recording); the legs are not drawn at all when it exceeds 6.
    pub clip: u16,
}

impl PoseParams {
    /// The two record offsets, `(body, legs)`, from the tables in the DS image.
    ///
    /// `FUN_26c0_0c93`, read instruction by instruction (`CS:cf4..d53`, `CS:dff..e54`):
    ///
    /// ```text
    /// cf4  si = 0x32AB + 0x2EF8 + 0x2F0C
    /// d03  if (raised) { if (0x32CF >= 0x1C) goto d26; goto d44 }     ; >= 0x1C: no frame
    /// d16  else if (0x2EF8 == 0x78 or 0x7E) goto d44; else goto d26
    /// d44  si += 0x2EFC                                               ; the frame IS added
    /// d26  (no frame)
    /// d7b  body = word[0x2628 + si]
    /// ```
    /// and for the legs:
    ///
    /// ```text
    /// e03  si = word[0x2622 + 0x2F0C]                                 ; by direction
    /// e0c  if (0x2F2C == 7) si += 0x12
    /// e17  if (raised) {
    /// e1f      if (0x32CF == 0x1C) si += word[0x2D22 + 0x2EFC]        ; THE WALK CYCLE
    ///          else               si += 2
    ///      } else                 si += 0x2EFC
    /// e4f  si += 0x32AB
    /// e54  legs = word[0x2628 + si]
    /// ```
    ///
    /// So the two branches are **complementary**: the pose that adds the frame to the body
    /// (`d44`) is the one that leaves the legs at a fixed record, and the states `0x1C..0x20` that
    /// freeze the body are the ones whose legs come from the walk-cycle table. Stage 9a had the
    /// body's branch right and the legs always at `+2`, which is why the climb-out after the
    /// throw stood still.
    pub fn records(&self, ds_img: &[u8]) -> (u16, u16) {
        // **The drawn frame is the frame AFTER `CS:0a06`'s step.** `FUN_26c0_277c` seeds
        // `0x2EFC = -2` when a state starts, and the same frame's tail adds `0x2F08` (2) to it
        // *before* the sprite is drawn, so `-2` is never a drawn value. Drawing it is not
        // cosmetic: the climb-out (`0x1C`) takes its legs step from `word[0x2D22 + frame]`, and at
        // `-2` that word is `word[0x2D20] = 0x6C` — the *carrying* pose block, which paints a
        // briefcase at the spy's feet for the one or two ticks the seed frame lasts (the owner's
        // post-throw glitch; the original never shows it). The laugh (`0x0C`) adds the frame to
        // its *body* record on the `d44` branch and reads two bytes short in the same way.
        let frame = if self.frame == SEED_FRAME { 0 } else { self.frame };
        let mut si = (self.extra as usize)
            .wrapping_add(self.set as usize)
            .wrapping_add(self.dir as usize)
            & 0xFFFF;
        if self.body_takes_frame() {
            si = (si + frame as usize) & 0xFFFF;
        }
        let body = SPRITE_BASE.wrapping_add(u16le(ds_img, (DS_POSE_TABLE + si) & 0xFFFF));
        let mut sj = u16le(ds_img, (DS_POSE_ROW + self.dir as usize) & 0xFFFF) as usize;
        if self.row_shift {
            sj += 0x12;
        }
        let step = if !self.raised {
            frame as usize
        } else if self.state == STATE_CLOSE {
            u16le(ds_img, (DS_LEGS_WALK + frame as usize) & 0xFFFF) as usize
        } else {
            2
        };
        sj = (sj + step + self.extra as usize) & 0xFFFF;
        let legs = SPRITE_BASE.wrapping_add(u16le(ds_img, (DS_POSE_TABLE + sj) & 0xFFFF));
        (body, legs)
    }

    /// `CS:d03..d53` — is the walk frame added to the **body** record? Only on the branch that
    /// skips the `sub $9` (`d44`): `raised` and below `0x1C`, or not raised and a `0x78`/`0x7E` set.
    pub fn body_takes_frame(&self) -> bool {
        (self.raised && self.state < STATE_CLOSE) || (!self.raised && (self.set == 0x78 || self.set == 0x7E))
    }

    /// Where the body's bottom row goes and how many rows the blitter drops from the **top** of
    /// the record (`0x31B2`). `CS:d26..d58`:
    ///
    /// ```text
    /// bx = y - 9
    /// if (0x2F68 < 9)  bx += 0x2F68                     ; the body simply sits lower
    /// else           { bx += 9; 0x31B2 = 0x2F68 - 9 }    ; the bottom is pinned, rows cut
    /// ```
    /// and the clip is *not* applied at all on the `d44` branch (the laugh), where the body sits on
    /// `Y` (the `sub $9` was skipped).
    pub fn body_place(&self, y: i32) -> (i32, i32) {
        if self.body_takes_frame() {
            // `CS:d73`: only the laugh (`0x0C`) and `0x14` pass the clip to `0x31B2` on this
            // branch; every other `d44` pose ignores it.
            let shift = if self.raised && (self.state == 0x0C || self.state == 0x14) {
                self.clip_value()
            } else {
                0
            };
            return (y, shift);
        }
        let clip = self.clip_value();
        if clip < BODY_CLIP {
            (y - 9 + clip, 0)
        } else {
            (y, clip - BODY_CLIP)
        }
    }

    /// The legs always end on `Y` (`CS:dde` reloads it), shifted down by the clip, whose rows the
    /// blitter then drops from the bottom (`0x31B2 = 0x2F68`, `CS:ddb`).
    pub fn legs_place(&self, y: i32) -> (i32, i32) {
        (y, self.clip_value())
    }

    /// `DS:0x2F68` as a signed row offset (the recordings only carry 0..0x1C).
    fn clip_value(&self) -> i32 {
        self.clip as i32
    }

    /// The legs are drawn at all only on two of the code's paths (`CS:d95..dbe`): `raised` with
    /// `state >= 0x1C` (the hatch states), or not raised with a set other than `0x78`/`0x7E`/`0x72`
    /// (the climb-down poses), and then only while the clip leaves the six rows `CS:dc9` tests.
    pub fn legs_drawn(&self) -> bool {
        if self.raised && self.state < STATE_CLOSE {
            return false;
        }
        if !self.raised && matches!(self.set, 0x78 | 0x7E | 0x72) {
            return false;
        }
        self.clip_value() <= LEGS_CLIP_MAX
    }
}

/// Read one spy's pose parameters out of a data-segment dump.
pub fn read_pose(ds: &[u8], player: usize) -> PoseParams {
    let o = 2 * player;
    PoseParams {
        dir: u16le(ds, DS_POSE + o),
        set: u16le(ds, DS_POSE_SET + o),
        frame: u16le(ds, DS_POSE_FRAME + o),
        extra: u16le(ds, DS_POSE_EXTRA + o),
        mirror: u16le(ds, DS_MIRROR + o) != 0,
        row_shift: u16le(ds, DS_POSE_ROW_SHIFT + o) == 7,
        raised: u16le(ds, DS_POSE_RAISED + o) != 0,
        state: u16le(ds, DS_POSE_STATE + o),
        clip: u16le(ds, DS_POSE_CLIP + o),
    }
}

/// The walk-cycle table an engine needs, as the game itself computes it: for each of the three
/// walking directions and both legs frames, the `(body, legs)` record offsets in the spy bank.
/// `set` is `[0x2EF8]` and `extra` `[0x32AB]`, the two values the engine keeps per spy; the plain
/// walking set is 36.
pub fn pose_table(ds_img: &[u8], set: u16, extra: u16) -> [[(u16, u16); 2]; 3] {
    let dirs = [0u16, 2, 4];
    let mut out = [[(0u16, 0u16); 2]; 3];
    for (i, &dir) in dirs.iter().enumerate() {
        for (f, &frame) in [2u16, 4].iter().enumerate() {
            let p = PoseParams {
                dir,
                set,
                frame,
                extra,
                mirror: false,
                row_shift: false,
                raised: false,
                state: 0,
                clip: 0,
            };
            out[i][f] = p.records(ds_img);
        }
    }
    out
}

/// The stage-1 hand-observed record map, kept for callers that used it. `spy_records_for` /
/// [`PoseParams::records`] supersede it: the same three bodies and overlays come out of the tables.
pub fn spy_records(pose: u16) -> Option<(u16, [u16; 2])> {
    match pose {
        0 => Some((0x7DC0, [0x7E5A, 0x836A])), // facing left/right, walk cycle overlays
        2 => Some((0x7F36, [0x872C, 0x889E])), // up
        4 => Some((0x7EA4, [0x872C, 0x87C0])), // down
        _ => None,
    }
}

/// File offset of the spy's sprite bank for `player`, from the dump's `DS:0x327B`.
pub fn spy_bank_offset(ds_img: &[u8], player: usize) -> Option<usize> {
    match u16le(ds_img, DS_SPY_BANK + 2 * player) {
        0x137A => Some(BANK_WHITE_OFF),
        0x16CA => Some(BANK_BLACK_OFF),
        _ => None,
    }
}

/// The two bank segments the round start writes into `DS:0x327B` / `DS:0x327D`: the white spy's
/// bank for player 1 and the black spy's for player 2. Measured: those two values are what every
/// recorded dump holds, and the shipped image holds 0 there because the assignment happens at run
/// time.
pub const SPY_BANK_SEG_WHITE: u16 = 0x137A;
pub const SPY_BANK_SEG_BLACK: u16 = 0x16CA;

/// The extra record-table byte offset the round start gives each player: `DS:0x32AB` = 0 for player
/// 1 and `DS:0x32AD` = 0x2EC for player 2, **measured** over 3,998 recorded dumps (no other value
/// occurs). It is part of the pose index, so it is what points player 2 at the black spy's records.
pub const SPY_POSE_EXTRA: [u16; 2] = [0, 0x2EC];

/// Write one spy's pose parameters into a data-segment image, exactly where `FUN_26c0_0c93` reads
/// them. This is the counterpart of [`read_pose`], and it is what lets an engine drive the sprite
/// layer from its own state instead of from a dump.
pub fn write_pose(ds: &mut [u8], player: usize, p: &PoseParams) {
    let o = 2 * player;
    let put = |ds: &mut [u8], addr: usize, v: u16| {
        ds[addr + o..addr + o + 2].copy_from_slice(&v.to_le_bytes());
    };
    put(ds, DS_POSE, p.dir);
    put(ds, DS_POSE_SET, p.set);
    put(ds, DS_POSE_FRAME, p.frame);
    put(ds, DS_POSE_EXTRA, p.extra);
    put(ds, DS_POSE_ROW_SHIFT, if p.row_shift { 7 } else { POSE_ROW_SHIFT_OFF });
    put(ds, DS_MIRROR, u16::from(p.mirror));
    put(ds, DS_POSE_RAISED, u16::from(p.raised));
    put(ds, DS_POSE_STATE, p.state);
    put(ds, DS_POSE_CLIP, p.clip);
}

/// Materialise the round-start values that `spy.exe`'s data segment does **not** hold.
///
/// The image is a snapshot of the data segment as the *setup* screen left it, so three classes of
/// field are wrong for a round and have to be written before anything is composed:
///
/// * **the five object blocks** (`DS:0x2600` -> `0x53BC, 0x562C, 0x589C, 0x5B0C, 0x5D7C`, 624 bytes
///   each). In the image they are filled with `0x02`, which is a valid object code, so a naive
///   compose paints 72 spurious objects over the room — that is the "black-and-white fence" the
///   live view showed instead of the snowfield. Every recorded dump holds *runtime* blocks here
///   (a handful of item codes), i.e. this area is built at round start: `FUN_26c0_125f` fills all
///   twenty rows with the 156-byte template row at `DS:0x2E1C` — which is all zeros — and then
///   `FUN_26c0_257b` places the island's objects (`place_round_objects`). Measured: 0x02 filler in
///   the image, sparse runtime codes in all 18,812 recorded dumps, blocks 0/1 (screens 0/2) all
///   zero in every island-1 dump (their rows are all wall, so the placement refuses the band).
/// * **the spy banks** (`DS:0x327B`, `DS:0x327D`): the image holds 0, so `draw_spy` cannot resolve a
///   bank and silently draws nothing. The round start writes 0x137A (white) and 0x16CA (black).
/// * **the pose defaults** (`DS:0x2EF8` set 0x24, `DS:0x2EFC` frame 2, `DS:0x32AB` extra 0 for
///   player 1 and 0x2EC for player 2, `DS:0x2F18`/`0x32CF`/`0x2F68` zero, `DS:0x2F64` zero). The
///   set/frame/raised/clip values match the image; they are written anyway so the pose state is a
///   function of this call rather than of the image's mood. The `extra` word is the one that is
///   *not* in the image: **measured** — `DS:0x32AB` is 0 and `DS:0x32AD` is 748 (0x2EC) in all 3,998
///   dumps checked, i.e. the round start gives player 2 an 0x2EC-byte offset into the same record
///   table. Without it player 2's body and legs resolve to player 1's records.
pub fn materialise_round_start(ds: &mut [u8]) {
    for i in 0..5usize {
        let base = u16le(ds, DS_BLOCK_PTR + 2 * i) as usize;
        if base + 624 <= ds.len() {
            ds[base..base + 624].fill(0);
        }
    }
    ds[DS_SPY_BANK..DS_SPY_BANK + 2].copy_from_slice(&SPY_BANK_SEG_WHITE.to_le_bytes());
    ds[DS_SPY_BANK + 2..DS_SPY_BANK + 4].copy_from_slice(&SPY_BANK_SEG_BLACK.to_le_bytes());
    for player in 0..2 {
        write_pose(
            ds,
            player,
            &PoseParams {
                dir: 0,
                set: 0x24,
                frame: 2,
                extra: SPY_POSE_EXTRA[player],
                mirror: false,
                row_shift: false,
                raised: false,
                state: 0,
                clip: 0,
            },
        );
    }
}

/// The height of a spy record in the bank (`[h][w]` at `bank_off + rec`), for reporting.
fn record_height(exe: &[u8], bank_off: usize, rec: u16) -> usize {
    let at = bank_off + rec as usize;
    if at + 1 < exe.len() {
        exe[at] as usize
    } else {
        0
    }
}

/// `DS:0x6F66` — the thrown snowball's sprite record, in the object-layer format (`[h][w]` then a
/// mask plane and a data plane), which is what `CS:3130` hands the blitter: 6 rows x 2 bytes.
pub fn ball_record(ds_img: &[u8]) -> Option<Record<'static>> {
    let at = BALL_RECORD;
    if at + 2 > ds_img.len() {
        return None;
    }
    Record::expand_planes(&ds_img[at..]).map(|r| Record {
        height: r.height,
        width: r.width,
        and: r.and.clone(),
        or: r.or.clone(),
        _src: &[],
    })
}

/// Draw one spy. Returns the record offsets used, or `None` if the spy was not drawn.
///
/// The sprite layer is skipped entirely when `DS:0x2F7E` is set for this player (the object routine
/// returns on that flag at its first instruction, and measured over the recorded population the
/// strip in those frames is the plates alone). `DS:0x2F64 >= 2` skips it too, as the spy routine's
/// own first test does.
pub fn draw_spy(
    strip: &mut [u8; STRIP_LEN],
    exe: &[u8],
    ds_img: &[u8],
    st: &RoundState,
    player: usize,
) -> Option<(u16, u16)> {
    let p = &st.players[player];
    if p.blank {
        return None;
    }
    draw_spy_at(strip, exe, ds_img, st, player, player)
}

/// Draw one spy's sprite into a console **other than his own** — the visitor, as
/// `FUN_26c0_0c93` does when the other spy's flag is set. `spy` supplies the pose (his own
/// `0x2EF8`/`0x2EFC`/`0x2F0C`/`0x32AB`/`0x2F14` and his own sprite bank); `console` supplies the
/// `map_x` the room is composed against and the `DS:0x2F64` state gate.
///
/// The blank flag is deliberately not consulted: the visitor's own console is the flat plate, but
/// his sprite still stands in the host's room.
pub fn draw_spy_at(
    strip: &mut [u8; STRIP_LEN],
    exe: &[u8],
    ds_img: &[u8],
    st: &RoundState,
    spy: usize,
    console: usize,
) -> Option<(u16, u16)> {
    let p = &st.players[spy];
    if u16le(ds_img, DS_SPY_STATE + 2 * spy) >= 2 {
        return None;
    }
    let bank_off = spy_bank_offset(ds_img, spy)?;
    if exe.len() <= bank_off + 0x4000 {
        return None;
    }
    let bank = &exe[bank_off..(bank_off + 0x10000).min(exe.len())];
    let pose = read_pose(ds_img, spy);
    let (body, legs) = pose.records(ds_img);
    // x is measured against the console's own scroll, whatever spy is being drawn
    let map_x = st.players[console].map_x;
    let x = p.spy.x as i32 - map_x as i32;
    let y = p.spy.y as i32;
    let (body_bottom, body_shift) = pose.body_place(y);
    let (legs_bottom, legs_shift) = pose.legs_place(y);
    let mut drew = false;
    if (body as usize) < bank.len() {
        if let Some(rec) = Record::expand_selfmasked(ds_img, &bank[body as usize..]) {
            masked_blit_clipped(strip, &rec, x, body_bottom, pose.mirror, body_shift);
            drew = true;
        }
    }
    if pose.legs_drawn() && (legs as usize) < bank.len() {
        if let Some(rec) = Record::expand_selfmasked(ds_img, &bank[legs as usize..]) {
            masked_blit_clipped(strip, &rec, x, legs_bottom, pose.mirror, legs_shift);
            drew = true;
        }
    }
    if drew {
        Some((body, legs))
    } else {
        None
    }
}


// ---- frame composition -------------------------------------------------
/// Which optional layers `render_frame`/`--check` draw. Both default to **on**: since stage 2A each
/// one is reproducible from the executable's own tables, and together they close 829 of 1082
/// recorded strips byte for byte.
pub struct Layers {
    pub objects: bool,
    pub spies: bool,
}

impl Default for Layers {
    fn default() -> Self {
        Layers { objects: true, spies: true }
    }
}

impl Layers {
    /// Plates only -- what stage 1 called the default.
    pub fn plates_only() -> Self {
        Layers { objects: false, spies: false }
    }
}

/// The full 320x200 frame: `bg` (64000 palette indices) with the room viewport(s) drawn in. The
/// surrounding console art is a static plate, not a reproduction of the round loop's own drawing;
/// `None` gives a black surround so the viewport stands alone.
pub fn render_frame(
    exe: &[u8],
    st: &RoundState,
    bg: Option<&[u8]>,
    both: bool,
    layers: &Layers,
) -> Screen {
    let mut screen = Screen {
        idx: match bg {
            Some(b) if b.len() >= crate::PIXELS => b[..crate::PIXELS].to_vec(),
            _ => vec![0u8; crate::PIXELS],
        },
    };
    let art = match Art::from_exe(exe) {
        Ok(a) => a,
        Err(_) => return screen,
    };
    let ds_img = &st.ds[..0x10000.min(st.ds.len())];
    let n = if both { 2 } else { 1 };
    for (i, &(vx, vy)) in [VIEW_TOP, VIEW_BOTTOM].iter().take(n).enumerate() {
        let (mut strip, _) = compose_passes(&art, st, i);
        if layers.objects {
            draw_objects(&mut strip, ds_img, st, i);
        }
        if layers.spies {
            draw_spy(&mut strip, exe, ds_img, st, i);
        }
        let idx = strip_to_indices(&strip);
        for row in 0..STRIP_ROWS {
            for col in 0..176 {
                let (x, y) = (vx + col, vy + row);
                if x < crate::W && y < crate::H {
                    screen.idx[y * crate::W + x] = idx[row * 176 + col];
                }
            }
        }
    }
    screen
}

/// Bounding box of the mismatching strip bytes, as `(rows, byte columns)`.
pub fn diff_box(strip: &[u8; STRIP_LEN], want: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let mut b: Option<(usize, usize, usize, usize)> = None;
    for i in 0..STRIP_LEN.min(want.len()) {
        if strip[i] != want[i] {
            let (r, c) = (i / STRIP_PITCH, i % STRIP_PITCH);
            b = Some(match b {
                None => (r, r, c, c),
                Some((r0, r1, c0, c1)) => (r0.min(r), r1.max(r), c0.min(c), c1.max(c)),
            });
        }
    }
    b
}

// ---- the player's TRAP.PIC -----------------------------------------------
// The round's console surround, as the engine starts each frame: `decode(TRAP.PIC)` with both
// viewport rects filled flat cyan (`crate::chrome`, `CS:0x29F3`, which reads the file straight into
// `B800:0000`).
//
// **The port carries no art.** Up to stage 28c this also held three captured `assets/*.bin` files
// (`round_bg`, `round_digits`, `round_kitbox`) as a fallback; stage 29 deleted both the files and
// every loader, so the only input left is the player's own `TRAP.PIC` and the four widgets, which
// are drawn from the records in `spy.exe` (`crate::chrome`).

/// Where the player's files are; fixed once per process by `main`. Unset in tests, which then read
/// the development path.
static ORIGINAL_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The round's screen art: one plate, derived from the player's `TRAP.PIC`.
pub struct RoundAssets {
    /// **The round plate, derived** (`crate::chrome::plate`): `decode(TRAP.PIC)` with both viewport
    /// rects filled flat cyan. `None` when the player's `TRAP.PIC` is not reachable (a checkout
    /// without the game, or `ANTICS_HIDE_ORIGINAL=1` under test), in which case the frame is built
    /// on an empty screen and only the viewports and widgets are drawn.
    pub plate: Option<Vec<u8>>,
}

impl RoundAssets {
    /// The directory the engine reads the player's files from. `main` fixes it once from the
    /// resolved lookup order (`--data-dir`/`--assets`/installed data dir/development path).
    pub fn set_original_dir(dir: &std::path::Path) {
        let _ = ORIGINAL_DIR.set(dir.to_path_buf());
    }

    /// The resolved directory, else the development path beside this checkout.
    pub fn original_dir() -> std::path::PathBuf {
        ORIGINAL_DIR.get().cloned().unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../spy-vs-spy-iii/original/256")
        })
    }

    /// `dir`'s `TRAP.PIC`, if it holds one -- the round screen and the selector's screen. Under
    /// `cargo test`, `ANTICS_HIDE_ORIGINAL=1` makes this answer `None` exactly as if the file were
    /// not there.
    pub fn trap_pic_in(dir: &std::path::Path) -> Option<Vec<u8>> {
        if hidden_for_tests() {
            return None;
        }
        match fs::read(dir.join(crate::chrome::TRAP_PIC_NAME)) {
            Ok(b) if b.len() >= crate::chrome::TRAP_PIC_BYTES => Some(b),
            _ => None,
        }
    }

    /// The player's `TRAP.PIC` out of the resolved directory, else out of the development path.
    pub fn trap_pic() -> Option<Vec<u8>> {
        RoundAssets::trap_pic_in(&RoundAssets::original_dir()).or_else(|| {
            RoundAssets::trap_pic_in(
                &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../spy-vs-spy-iii/original/256"),
            )
        })
    }

    pub fn load() -> RoundAssets {
        let plate = RoundAssets::trap_pic().and_then(|raw| crate::chrome::plate(&raw));
        RoundAssets { plate }
    }

    /// [`load`](Self::load) with the directory named, so a test can compare two of them (the
    /// development copy against an `--install`ed one) without touching the process-wide setting.
    pub fn load_from(dir: &std::path::Path) -> RoundAssets {
        let plate = RoundAssets::trap_pic_in(dir).and_then(|raw| crate::chrome::plate(&raw));
        RoundAssets { plate }
    }
}

// ---- CLI ---------------------------------------------------------------
pub struct RoundCli {
    pub exe: String,
    pub ds: String,
    pub shot: Option<String>,
    pub state: Option<String>,
    pub check: bool,
    pub both: bool,
    pub layers: Layers,
}

/// `--scene round`: render, and optionally check, the room viewport for a recorded data-segment
/// dump. Returns text lines to print (the check report) on success.
pub fn run(cli: &RoundCli) -> Result<Vec<String>, String> {
    let exe = fs::read(&cli.exe).map_err(|e| format!("{}: {e}", cli.exe))?;
    let st = RoundState::from_ds_dump(Path::new(&cli.ds))?;
    let art = Art::from_exe(&exe)?;
    let mut report = Vec::new();

    if cli.check {
        report.push(format!(
            "exe={} bytes  art_segment@file {:#x} ({} of 65536 bytes ship in the image)",
            exe.len(),
            ART_FILE_OFF,
            art.static_len
        ));
        let want = &st.ds[DS_STRIP..DS_STRIP + STRIP_LEN];
        let n = if cli.both { 2 } else { 1 };
        let ds_img = &st.ds[..0x10000.min(st.ds.len())];
        for player in 0..n {
            let p = &st.players[player];
            let (strip, passes) = compose_passes(&art, &st, player);
            let mut layers = strip;
            let mut n_obj = 0;
            let mut spy_recs: Option<(u16, u16)> = None;
            if cli.layers.objects {
                n_obj = draw_objects(&mut layers, ds_img, &st, player);
            }
            if cli.layers.spies {
                spy_recs = draw_spy(&mut layers, &exe, ds_img, &st, player);
            }
            let plate_diff = strip.iter().zip(want).filter(|(a, b)| a != b).count();
            let diff = layers.iter().zip(want).filter(|(a, b)| a != b).count();
            let box_ = diff_box(&layers, want);
            // Where the spy sits, from the state alone: the body record ends at row Y-9 and the legs
            // overlay at row Y, both 8 bytes wide starting at x = spy.x - map_x (or the other
            // player's map_x when this spy is visiting). Anything outside this box is the room.
            let pose = read_pose(ds_img, player);
            let (body_rec, legs_rec) = pose.records(ds_img);
            let map_x = if p.blank { st.players[player ^ 1].map_x } else { p.map_x };
            let sx = p.spy.x as i32 - map_x as i32;
            let sy = p.spy.y as i32;
            let body_h = spy_bank_offset(ds_img, player)
                .map(|o| record_height(&exe, o, body_rec))
                .unwrap_or(0);
            let (bx0, bx1) = (sy - 9 - body_h as i32, sy - 1);
            let outside = (0..STRIP_LEN)
                .filter(|&i| layers[i] != want[i])
                .filter(|&i| {
                    let (r, c) = ((i / STRIP_PITCH) as i32, (i % STRIP_PITCH) as i32);
                    !(r >= bx0 && r <= bx1 && c >= sx && c < sx + 8)
                })
                .count();
            let inside = diff - outside;
            let pass_txt: Vec<String> = passes
                .iter()
                .map(|q| format!("cell@{} lvl{} plate{} cx{} dx{} w{}", q.cell_off, q.level, q.plate, q.cx, q.dx, q.width))
                .collect();
            report.push(format!(
                "player {} map_x={} band={} blank={} passes=[{}]",
                player + 1,
                p.map_x,
                p.band,
                p.blank,
                pass_txt.join("; ")
            ));
            report.push(format!(
                "  strip {} bytes: plates-only mismatch {} bytes; with layers {} bytes; box={}",
                STRIP_LEN,
                plate_diff,
                diff,
                match box_ {
                    Some((r0, r1, c0, c1)) => format!("rows {r0}..{r1} cols {c0}..{c1}"),
                    None => "none".to_string(),
                }
            ));
            report.push(format!(
                "  outside the spy box (rows {}..{} cols {}..{}): {} bytes; inside: {}",
                bx0, bx1, sx, sx + 7, outside, inside
            ));
            report.push(format!(
                "  layers: objects={} ({} drawn) spies={} ({}); pose_dir={}",
                cli.layers.objects,
                n_obj,
                cli.layers.spies,
                match (cli.layers.spies, spy_recs) {
                    (false, _) => "off".to_string(),
                    (true, Some((b, l))) => format!("body {b:#06x} legs {l:#06x}"),
                    (true, None) => "skipped".to_string(),
                },
                p.spy.pose
            ));
            report.push(format!(
                "  pose params: set={} dir={} frame={} extra={} mirror={} -> body {:#06x} legs {:#06x}",
                pose.set, pose.dir, pose.frame, pose.extra, pose.mirror, body_rec, legs_rec
            ));
        }
    }

    if let Some(path) = &cli.shot {
        // the surround is the round's own plate -- `decode(TRAP.PIC)` -- not the setup screen this
        // view used up to stage 28c; the port reads no art of its own (stage 29)
        let bg = RoundAssets::trap_pic().and_then(|raw| crate::chrome::plate(&raw));
        let screen = render_frame(&exe, &st, bg.as_deref(), cli.both, &cli.layers);
        crate::write_bmp(Path::new(path), &screen, &ROUND_PALETTE)
            .map_err(|e| e.to_string())?;
        report.push(format!("wrote {path}"));
    }
    if let Some(path) = &cli.state {
        let p = &st.players[0];
        let json = format!(
            "{{\"frame\":0,\"scene\":\"round\",\"screen_id\":{},\"map_x\":{},\"band\":{},\"levels\":[{}],\"spy\":{{\"x\":{},\"y\":{},\"pose\":{},\"mirror\":{}}},\"blank\":{}}}\n",
            st.screen_id,
            st.map_x,
            p.band,
            st.levels[0].iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","),
            p.spy.x,
            p.spy.y,
            p.spy.pose,
            p.spy.mirror,
            p.blank
        );
        fs::write(path, json).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }
    fn exe_bytes() -> Vec<u8> {
        fs::read(crate::orig::require("spy-vs-spy-iii/original/256/spy.exe")).expect("spy.exe")
    }
    fn dump(ses: &str, name: &str) -> Option<RoundState> {
        let p = crate::orig::require("recon/round/sessions").join(ses).join(name);
        if !p.exists() {
            eprintln!("skip: {} not present", p.display());
            return None;
        }
        Some(RoundState::from_ds_dump(&p).expect("parse dump"))
    }

    /// The strip the model composes and the strip the real game left in the same dump.
    fn residuals(st: &RoundState, exe: &[u8], player: usize) -> (usize, usize) {
        let art = Art::from_exe(exe).unwrap();
        let (strip, _) = compose_passes(&art, st, player);
        let want = &st.ds[DS_STRIP..DS_STRIP + STRIP_LEN];
        let p = &st.players[player];
        let sx = p.spy.x as i32 - p.map_x as i32;
        let (b0, b1) = (p.spy.y as i32 - 28, p.spy.y as i32 - 1);
        let mut out = 0;
        for i in 0..STRIP_LEN {
            if strip[i] != want[i] {
                let (r, c) = ((i / STRIP_PITCH) as i32, (i % STRIP_PITCH) as i32);
                if !(r >= b0 && r <= b1 && c >= sx && c < sx + 8) {
                    out += 1;
                }
            }
        }
        (out, strip.iter().zip(want).filter(|(a, b)| a != b).count())
    }

    #[test]
    fn art_segment_and_tables_are_the_static_image() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        let art = Art::from_exe(&exe).unwrap();
        assert_eq!(ART_FILE_OFF, 0x12160);
        // 37843 of the segment's 65536 bytes ship in the image; the rest is past end-of-file.
        assert_eq!(art.static_len, 37843);
        // plate 0x46B0 is the flat cyan plate the blanking branch resolves to (all 0x55);
        // plate 0 (the level-8 plate) is real artwork and is not flat.
        assert!(art.plate(0x46B0).iter().all(|&b| b == 0x55));
        assert!(!art.plate(0).iter().all(|&b| b == 0x55));
        // every one of the 26 plate offsets is inside the static part of the segment
        let st = match dump("playtest1", "ds_00521.bin") { Some(s) => s, None => return };
        for &p in st.plate.iter() {
            assert!((p as usize) + 1508 <= art.static_len, "plate {p:#x} past end of image art");
        }
        // the tables the blit indexes are static: a dump agrees with the image byte for byte
        let ds_img = &exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000];
        assert_eq!(&st.gate.iter().map(|v| v.to_le_bytes()).flatten().collect::<Vec<_>>()[..],
                   &ds_img[DS_GATE..DS_GATE + 24]);
        assert_eq!(&st.row_base.iter().map(|v| v.to_le_bytes()).flatten().collect::<Vec<_>>()[..],
                   &ds_img[DS_ROW_BASE..DS_ROW_BASE + 16]);
        assert_eq!(&st.plate.iter().map(|v| v.to_le_bytes()).flatten().collect::<Vec<_>>()[..],
                   &ds_img[DS_PLATE..DS_PLATE + 52]);
    }

    #[test]
    fn gate_scan_and_pass_widths() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let st = match dump("playtest1", "ds_00521.bin") { Some(s) => s, None => return };
        assert_eq!(st.map_x, 126);
        assert_eq!(st.gate_col(126), 4); // 4 * 26 = 104 <= 126 < 130
        assert_eq!(st.gate[4], 104);
        let exe = exe_bytes();
        let art = Art::from_exe(&exe).unwrap();
        let (_, passes) = compose_passes(&art, &st, 0);
        assert_eq!(passes.len(), 3);
        assert_eq!(passes[0].cx, 22); // 126 - 104
        assert_eq!(passes.iter().map(|p| p.width as u32).sum::<u32>(), 44);
        assert_eq!(passes[0].width, 4);
        assert_eq!(passes[1].dx, 4);
        assert_eq!(passes[2].dx, 30);
    }

    #[test]
    fn composed_strip_reproduces_recorded_strips_exactly() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        // Three dumps whose whole 2552-byte strip the port reproduces with 0 differences.
        for (ses, name) in [
            ("playtest1", "ds_00405.bin"),
            ("playtest2", "ds_00600.bin"),
            ("playtest5", "ds_00810.bin"),
        ] {
            if let Some(st) = dump(ses, name) {
                let (outside, total) = residuals(&st, &exe, 0);
                assert_eq!((outside, total), (0, 0), "{ses}/{name}");
            }
        }
    }

    #[test]
    fn residual_is_the_spy_sprite_only() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        // Two dumps where the spy stands in the view: the room is exact, the bytes that differ are
        // all inside the box where the (not yet drawn) spy sprite sits.
        for (ses, name, expect_max) in [
            ("playtest1", "ds_00521.bin", 100usize),
            ("playtest4", "ds_00060.bin", 120),
            ("playtest2", "ds_00015.bin", 120),
        ] {
            if let Some(st) = dump(ses, name) {
                let (outside, total) = residuals(&st, &exe, 0);
                assert_eq!(outside, 0, "{ses}/{name}: {outside} bytes differ outside the spy box");
                assert!(
                    total > 0 && total <= expect_max,
                    "{ses}/{name}: total residual {total} outside 1..={expect_max}"
                );
            }
        }
    }

    #[test]
    fn blanked_console_branch_is_exact() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        if let Some(st) = dump("rocket-room-2", "ds_00690.bin") {
            assert!(st.players[0].blank, "0x2F7E must be set for this dump");
            let (outside, total) = residuals(&st, &exe, 0);
            assert_eq!((outside, total), (0, 0));
            let (strip, passes) = compose_passes(&Art::from_exe(&exe).unwrap(), &st, 0);
            assert_eq!(passes[0].plate, 0x46B0); // the flat plate
            assert!(strip.iter().all(|&b| b == 0x55));
        }
    }

    #[test]
    fn second_player_uses_the_interleaved_fields() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        if let Some(st) = dump("playtest1", "ds_00521.bin") {
            assert_eq!(st.players[0].map_x, u16::from_le_bytes([st.ds[0x24A6], st.ds[0x24A7]]));
            assert_eq!(st.players[1].map_x, u16::from_le_bytes([st.ds[0x24A8], st.ds[0x24A9]]));
            assert_eq!(st.players[1].band, u16::from_le_bytes([st.ds[0x32A1], st.ds[0x32A2]]));
            assert_ne!(st.players[0].map_x, st.players[1].map_x);
        }
    }

    #[test]
    fn pixel_unpacking_and_palette() {
        let mut strip = [0u8; STRIP_LEN];
        strip[0] = 0b00_01_10_11; // pixels 0,1,2,3 across the top-left of row 0
        let idx = strip_to_indices(&strip);
        assert_eq!(&idx[0..4], &[0, 1, 2, 3]);
        assert_eq!(idx.len(), 176 * 58);
        let rgb = strip_to_rgb(&strip);
        assert_eq!(rgb[0], ROUND_PALETTE[0]);
        assert_eq!(rgb[1], [0x55, 0xFF, 0xFF]); // cyan
        assert_eq!(rgb[2], [0xFF, 0x55, 0xFF]); // magenta
        assert_eq!(rgb[3], [0xFF, 0xFF, 0xFF]); // white
    }

    #[test]
    fn mirror_reverses_pixels_not_planes() {
        assert_eq!(reverse_pixels(0x1B), 0xE4);
        assert_eq!(reverse_pixels(0xC8), 0x23);
        for b in 0..=255u8 {
            assert_eq!(reverse_pixels(reverse_pixels(b)), b);
        }
    }

    #[test]
    fn frame_puts_the_viewport_at_80_10() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        let st = match dump("playtest1", "ds_00521.bin") { Some(s) => s, None => return };
        let screen = render_frame(&exe, &st, None, false, &Layers::plates_only());
        let strip = compose_view(&exe, &st);
        let idx = strip_to_indices(&strip);
        for row in 0..STRIP_ROWS {
            for col in 0..176 {
                assert_eq!(screen.idx[(VIEW_TOP.1 + row) * crate::W + VIEW_TOP.0 + col],
                           idx[row * 176 + col]);
            }
        }
        assert_eq!(screen.idx.len(), crate::PIXELS);
    }

    // ---- stage 2A: the sprite layers ------------------------------------

    /// plates + objects + spy, as `--check --objects --spies` composes it.
    fn compose_full(exe: &[u8], st: &RoundState, player: usize) -> [u8; STRIP_LEN] {
        let art = Art::from_exe(exe).unwrap();
        let (mut strip, _) = compose_passes(&art, st, player);
        let ds_img = &st.ds[..0x10000];
        draw_objects(&mut strip, ds_img, st, player);
        draw_spy(&mut strip, exe, ds_img, st, player);
        strip
    }

    fn residual(strip: &[u8; STRIP_LEN], want: &[u8]) -> usize {
        strip.iter().zip(want).filter(|(a, b)| a != b).count()
    }

    /// The pose map is derived from the tables, and it reproduces the three bodies and the four
    /// overlays the pixel solver recovered from the captured poses.
    #[test]
    fn pose_table_derives_the_walk_records() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        let ds_img = &exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000];
        let t = pose_table(ds_img, 0x24, 0);
        assert_eq!(t[0][0].0, 0x7DC0); // dir 0 (horizontal), frame 2
        assert_eq!(t[0][1].0, 0x7DC0); // body does not change with the walk frame
        assert_eq!(t[0][0].1, 0x7E5A); // first legs overlay
        assert_eq!(t[0][1].1, 0x836A); // second legs overlay
        assert_eq!(t[1][0].0, 0x7F36); // up
        assert_eq!(t[2][0].0, 0x7EA4); // down
        // up and down share one overlay; the table lands on a duplicate of it
        let legs_up = t[1][0].1;
        let bank = &exe[BANK_WHITE_OFF..];
        let (h1, w1) = (bank[legs_up as usize] as usize, bank[legs_up as usize + 1] as usize);
        let (h2, w2) = (bank[0x872C] as usize, bank[0x872D] as usize);
        assert_eq!((h1, w1), (h2, w2));
        assert_eq!(&bank[legs_up as usize..legs_up as usize + 2 + h1 * w1],
                   &bank[0x872C..0x872C + 2 + h2 * w2],
                   "the overlay the table selects must be the art the solver found");
        // and the legs index follows `0x2EFC`
        assert_ne!(t[0][0].1, t[0][1].1);
    }

    /// The two expansion tables ARE the transparency map: a source pixel of 1 preserves the
    /// destination, anything else replaces it with the source colour.
    #[test]
    fn selfmasked_expansion_semantics() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        let ds_img = &exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000];
        assert_eq!(ds_img[DS_AND_TAB + 0x55], 0xFF, "0x55 must be fully transparent");
        assert_eq!(ds_img[DS_OR_TAB + 0x55], 0x00);
        for v in 0..=255usize {
            let a = ds_img[DS_AND_TAB + v];
            let o = ds_img[DS_OR_TAB + v];
            for x in 0..=255u8 {
                let d = (x & a) | o;
                for k in 0..4 {
                    let sh = 6 - 2 * k;
                    let src = ((v >> sh) & 3) as u8;
                    let got = (d >> sh) & 3;
                    if src == 1 {
                        assert_eq!(got, (x >> sh) & 3, "v={v:#04x} x={x:#04x} pixel {k}");
                    } else {
                        assert_eq!(got, src, "v={v:#04x} x={x:#04x} pixel {k}");
                    }
                }
            }
        }
        // a record whose plane is all 0x55 paints nothing at all
        let plane = [1u8, 8u8].iter().copied().chain(std::iter::repeat(0x55u8).take(8)).collect::<Vec<u8>>();
        let rec = Record::expand_selfmasked(ds_img, &plane).unwrap();
        let mut strip = [0xA5u8; STRIP_LEN];
        let before = strip;
        masked_blit(&mut strip, &rec, 0, 8, false);
        assert_eq!(strip, before);
    }

    /// The object records are two raw planes, not one plane through the tables.
    #[test]
    fn object_records_are_raw_two_plane() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        let ds_img = &exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000];
        let rec = Record::expand_planes(&ds_img[DS_OBJ_FIXED..]).unwrap();
        let n = rec.height * rec.width;
        assert_eq!(rec.and, ds_img[DS_OBJ_FIXED + 2..DS_OBJ_FIXED + 2 + n].to_vec());
        assert_eq!(rec.or, ds_img[DS_OBJ_FIXED + 2 + n..DS_OBJ_FIXED + 2 + 2 * n].to_vec());
        // the first plane is a mask in final 2bpp form: every pixel is 0 (opaque) or 3 (transparent)
        for &b in &rec.and {
            for k in 0..4 {
                let px = (b >> (6 - 2 * k)) & 3;
                assert!(px == 0 || px == 3, "mask pixel {px} in byte {b:#04x}");
            }
        }
    }

    /// Whole strips, spies and objects included, are byte-identical for settled frames.
    #[test]
    fn complete_strips_are_byte_exact() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        for (ses, name) in [
            ("playtest1", "ds_00015.bin"),
            ("playtest4", "ds_00205.bin"),
            ("playtest4", "ds_00385.bin"), // band 18: past the end of the row-base table
            ("rocket-room-2", "ds_00060.bin"),
            ("playtest2", "ds_00605.bin"), // spy skipped by 0x2F7E
        ] {
            if let Some(st) = dump(ses, name) {
                let mut ok = false;
                for player in 0..2 {
                    let strip = compose_full(&exe, &st, player);
                    let want = &st.ds[DS_STRIP..DS_STRIP + STRIP_LEN];
                    if residual(&strip, want) == 0 {
                        ok = true;
                    }
                }
                assert!(ok, "{ses}/{name}: no player reproduced the strip");
            }
        }
    }

    /// A band past the row-base table must read through, not clamp: `[band+0x24AA]` is a byte
    /// offset, and the dumps contain band = 18.
    #[test]
    fn band_past_the_table_reads_through() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        let st = match dump("playtest4", "ds_00385.bin") { Some(s) => s, None => return };
        assert_eq!(st.players[0].band, 18);
        let art = Art::from_exe(&exe).unwrap();
        let (strip, passes) = compose_passes(&art, &st, 0);
        assert_eq!(passes[0].cell_off, 224); // 0xE0 = 0x18 (row base) + 2*col
        assert_eq!(residual(&strip, &st.ds[DS_STRIP..DS_STRIP + STRIP_LEN]), 100);
    }

    /// `0x2F7E` set means this player's room is not the one on the strip: no sprite layer at all.
    #[test]
    fn visit_flag_skips_the_sprite_layers() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe_bytes();
        let st = match dump("playtest2", "ds_00605.bin") { Some(s) => s, None => return };
        assert!(st.players[0].blank, "0x2F7E must be set for this dump");
        let art = Art::from_exe(&exe).unwrap();
        let (strip, _) = compose_passes(&art, &st, 0);
        let want = &st.ds[DS_STRIP..DS_STRIP + STRIP_LEN];
        assert_eq!(residual(&strip, want), 0, "plates alone are exact here");
        let full = compose_full(&exe, &st, 0);
        assert_eq!(residual(&full, want), 0, "and the sprite layers must not touch it");
    }

    // ---- stage 19b: the map's dots (`CS:2C60`) ---------------------------------------------

    /// The static image's data segment with all four position records cleared to `0xFFFF`, then
    /// the given `(slot, screen, entry)` records written. Slot 2/4/6/8 = carried code 9/10/11/12.
    fn ds_with_records(records: &[(usize, i16, u8)]) -> Vec<u8> {
        let exe = exe_bytes();
        let mut ds = exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000].to_vec();
        for slot in [2usize, 4, 6, 8] {
            ds[DS_POS_RECORD + slot..DS_POS_RECORD + slot + 2]
                .copy_from_slice(&0xFFFFu16.to_le_bytes());
            ds[DS_POS_COL + slot] = 0;
        }
        for &(slot, screen, entry) in records {
            ds[DS_POS_RECORD + slot..DS_POS_RECORD + slot + 2]
                .copy_from_slice(&(screen as u16).to_le_bytes());
            ds[DS_POS_COL + slot] = entry;
        }
        ds
    }

    /// **The dots have no "found" gate.** `CS:2C60` reads the record, the lattice row and the
    /// lattice column and nothing else — so the kit word (`0x32A3`/`0x32A5`), the deposited-parts
    /// mask (`0x2EB8`), what is in either hand (`0x2F2C`/`0x2F2E`) and the per-part owner
    /// (`0x322D`) move the dots not at all. A "hidden until found" dot would need a read that the
    /// routine does not have; this test is the regression that keeps one from being invented.
    #[test]
    fn the_part_dots_have_no_found_gate() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut ds = ds_with_records(&[(8, 2, 120), (6, 6, 64), (4, 4, 74), (2, 6, 38)]);
        let want = map_dots(&ds);
        assert_eq!(want.iter().map(|d| (d.slot, d.row, d.col)).collect::<Vec<_>>(),
                   vec![(8, 38, 26), (6, 22, 20), (4, 30, 20), (2, 22, 17)],
                   "slot 8 first (code 12), slot 2 last (code 9); row = lattice row - 2, \
                    col = lattice column + 1");
        for kit in [0x0000u16, 0x0040, 0x0080, 0x0200, 0x02C0, 0x0400, 0x0FFF] {
            for mask in [0x0400u16, 0x0440, 0x06C0] {
                for (hand_w, hand_b) in [(-1i16, -1i16), (12, 12), (9, 10)] {
                    ds[0x32A3..0x32A5].copy_from_slice(&kit.to_le_bytes());
                    ds[0x32A5..0x32A7].copy_from_slice(&kit.to_le_bytes());
                    ds[0x2EB8..0x2EBA].copy_from_slice(&mask.to_le_bytes());
                    ds[0x2F2C..0x2F2E].copy_from_slice(&hand_w.to_le_bytes());
                    ds[0x2F2E..0x2F30].copy_from_slice(&hand_b.to_le_bytes());
                    for slot in [2usize, 4, 6, 8] {
                        ds[0x322D + slot..0x322D + slot + 2]
                            .copy_from_slice(&(slot as u16).to_le_bytes());
                    }
                    assert_eq!(map_dots(&ds), want,
                               "kit {kit:#06x} mask {mask:#06x} hands {hand_w}/{hand_b} must not \
                                change a single dot");
                }
            }
        }
    }

    /// The dots of six recorded map frames, against the marker positions **the recorded CGA
    /// framebuffer shows**. `0x148C` is three rows of one byte, so a dot's marker starts three
    /// strip rows above the blit's `BX` the records produce; the pairs below are the marker's own
    /// top-left cells as `stage19b_dots.py` found them (`stage19b_dots.csv`, `seen_pos`).
    #[test]
    fn the_dots_are_the_ones_the_recorded_frames_show() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // (session, dump, what the recording shows, why it is worth pinning)
        let cases: [(&str, &str, &[(i32, i32)], &str); 6] = [
            ("playtest5", "ds_00114.bin", &[(19, 17), (19, 20), (27, 20)],
             "24 s into a 480 s round, kit 0x083a mask 0x0400: NO part found, and the original's \
              map still shows three dots"),
            ("playtest5", "ds_00185.bin", &[(19, 17), (19, 20), (27, 20)],
             "part 11 deposited (mask 0x0600): its own record is -1, so its dot is the one that \
              goes"),
            ("playtest5", "ds_00280.bin", &[(19, 17), (27, 20)],
             "parts 9 and 11 are in the case (mask 0x0640) and their records are -1, so their dots \
              are gone; the two left are the briefcase and part 10, which nobody has found"),
            ("playtest5", "ds_00621.bin", &[(19, 23), (19, 26), (19, 29), (43, 20)],
             "four dots, one of them the briefcase on screen 0"),
            ("playtest5", "ds_01254.bin", &[(43, 20)],
             "everything found (mask 0x0640): one record left, one dot"),
            ("playtest3", "ds_00254.bin", &[(19, 17), (19, 20)],
             "the attract demo's turn, two distinct dots from four records"),
        ];
        for (ses, name, shown, why) in cases {
            let st = match dump(ses, name) { Some(s) => s, None => continue };
            let mut got: Vec<(i32, i32)> = map_dots(&st.ds).iter().map(|d| (d.row, d.col)).collect();
            got.sort_unstable();
            got.dedup();
            // the marker's top row is three above the blit's `BX`
            let mut want: Vec<(i32, i32)> = shown.iter().map(|&(r, c)| (r + 3, c)).collect();
            want.sort_unstable();
            assert_eq!(got, want, "{ses}/{name}: {why}");
        }
    }

    /// `0x148C`'s twelve pixels are the same on every one of the four colours — the reason a
    /// recorded dot can be counted at all, and the reason the map window stays pixel-exact.
    #[test]
    fn the_dot_paints_the_same_twelve_pixels_on_any_colour() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // `and` = f3 c0 f3, `or` = fb ea fb over one byte, three rows: (dst & and) | or
        for bg in 0..4u8 {
            let mut ds = ds_with_records(&[(2, 6, 38)]);
            // the whole plate a single colour, so the dot's cell is background-independent
            let byte = bg * 0x55;
            for b in &mut ds[DS_MAP_PLATE + 2..DS_MAP_PLATE + 2 + MAP_PLATE_ROWS * MAP_PLATE_BYTES] {
                *b = byte;
            }
            // the map's own marker off (`CS:2CA3` needs 0x2F48 >= 0 to draw it)
            ds[DS_MAP_FLAG..DS_MAP_FLAG + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
            let mut strip = [0u8; STRIP_LEN];
            let (plate, legs, parts, own) = draw_map(&mut strip, &ds, 0);
            assert_eq!((plate, legs, parts, own), (1, 0, 1, 0),
                       "one dot, no legs, no own marker");
            // the dot: slot 2, screen 6, entry 38 -> strip (BX 22, CX 17), sprite rows 19..21
            for (i, want) in [0xFBu8, 0xEA, 0xFB].iter().enumerate() {
                let at = (19 + i) * STRIP_PITCH + 17;
                assert_eq!(strip[at], *want,
                           "row {} of the marker on background {bg}", i);
            }
        }
    }

    // ---- stage 22c: the clock's hundredth ------------------------------------------------------

    /// The modelled clock (`CLOCK_TICK_DRAWS`): one `(dh, dl)` is held for 96 draws, then the clock
    /// ticks five hundredths (a DOS tick is 5.49) and carries into the seconds field.
    ///
    /// This is what keeps `CS:257b`'s two unbounded re-rolls (the hatch cell search, the fire
    /// lottery) terminating. A *frozen* `DX` can never end them — measured: `--obj-clock` with
    /// `SystemTime` read on every draw came out of 1 launch in 40 with **no hatch at all**, because
    /// the hatch's 4096-iteration cap ran out inside a single hundredth.
    #[test]
    fn the_clocked_generator_ticks_one_hundredth_at_a_time() {
        let mut r = ObjRng::from_clock(Some((0x0A, 0x5E)));
        assert!(r.is_clock());
        assert_eq!((r.dh, r.dl), (0x0A, 0x5E));
        assert_eq!(r.byte(), ((0x5E_u8.wrapping_add(0x0A) as u16 * 0x5E) & 0xFF) as u8);
        // draws 2..95 are in the same hundredth (the 96th is the one that ticks)
        for _ in 0..CLOCK_TICK_DRAWS - 2 {
            r.byte();
        }
        assert_eq!(r.draws, CLOCK_TICK_DRAWS as u32 - 1);
        assert_eq!((r.dh, r.dl), (0x0A, 0x5E), "the hundredth holds for CLOCK_TICK_DRAWS draws");
        r.byte();
        assert_eq!((r.dh, r.dl), (0x0A, 0x63), "then it ticks by five hundredths");
        // the seconds field carries when the hundredths pass 99
        let mut c = ObjRng::from_clock(Some((0x0A, 0x5F)));
        for _ in 0..CLOCK_TICK_DRAWS {
            c.byte();
        }
        assert_eq!((c.dh, c.dl), (0x0B, 0x00), "95 + 5 = 100 -> 0 hundredths, one more second");
        // and the seconds wrap at 60, like INT 21h's DH
        let mut w = ObjRng::from_clock(Some((59, 0x5F)));
        for _ in 0..CLOCK_TICK_DRAWS {
            w.byte();
        }
        assert_eq!((w.dh, w.dl), (0, 0x00), "and the seconds wrap at 60, like INT 21h's DH");
    }

    /// A pinned generator never ticks: `--seed` and the attract round keep the fixed `0x770D` for
    /// ever, which is what makes every recorded replay byte-exact.
    #[test]
    fn a_pinned_generator_keeps_its_fixed_dx() {
        let mut a = ObjRng::from_seed(9);
        let mut b = ObjRng::at(a.state());
        assert!(!a.is_clock());
        assert_eq!((a.dh, a.dl), (RNG_FIXED_DH, RNG_FIXED_DL));
        for _ in 0..4096 {
            assert_eq!(a.byte(), b.byte());
        }
        assert_eq!(a.draws, 4096);
        // the injectable clock reading is inert without the clock
        let mut c = ObjRng::from_seed(9);
        for _ in 0..4096 {
            c.byte();
        }
        assert_eq!(a.state(), c.state());
    }


    /// **Stage 24a** — `DEMO.DAT` is one 16-bit key mask a frame, little endian, and the game reads
    /// `word[0x0FB6 + 0x0D]` with `0x0FB6` starting at 0 (`CS:0F44`). Checked against the recorded
    /// stream's own first words: five frames of nothing, then spy 1 walking down (bit 2), then the
    /// pair of "both spies' A and B" masks.
    #[test]
    fn demo_masks_parse_the_recorded_stream_layout() {
        let bytes = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x04, 0x00, // frame 5: spy 1 down
            0x04, 0x30, // frame 6: spy 1 down, both spies' A and B
            0x00, 0x00,
            0x30, 0x00, // spy 1 A and B only
            0x08,       // a trailing odd byte, dropped
        ];
        assert_eq!(demo_masks(&bytes), vec![0, 0, 0, 0, 0, 0x0004, 0x3004, 0, 0x0030]);
        assert_eq!(DEMO_MASK_BITS, 0x3F3F);
        // the real file, if the original's disk directory is where it always is
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../spy-vs-spy-iii/original/256/DEMO.DAT");
        if let Ok(b) = std::fs::read(&p) {
            let m = demo_masks(&b);
            assert_eq!(m.len(), 941, "1,882 bytes = 941 frames");
            assert_eq!(&m[..12], &[0, 0, 0, 0, 0, 4, 4, 4, 0x3004, 4, 4, 0x3004],
                       "the recorded stream's opening");
            assert!(m.iter().take(5).all(|m| *m == 0));
        }
    }

}
