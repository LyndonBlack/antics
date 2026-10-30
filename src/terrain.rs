//! The per-level **behaviour mask** — the original's collision and interaction map.
//!
//! This is the table `FUN_26c0_366e` reads and `FUN_26c0_1e0f` acts on, and it is the "wall veto"
//! earlier recon could not isolate (only its effect was measured). The chain, all `read` from
//! `recon/disasm/spy_decompiled.c` (function 0x366e at CS:366e; the disassembly of the caller is
//! `CS:096a..0979` in the main loop):
//!
//! ```text
//!   col      = the 26-byte cell containing x          (gate table DS:0x25AE = 0,26,..,286)
//!   level    = word[DS:0x24BE + 12*screen + 2*col]    (the island's level table)
//!   mask     = word[DS:0x25E0 + level]                (pointer, one per even level)
//!   row      = (y - 0x22) >> 1                        (y base 34, two rows per byte)
//!   code     = byte[mask + row*0x1A + (x - gate[col])]
//! ```
//!
//! Each mask is **26 bytes x 13 rows = 338 bytes**, one byte per (world x unit, 2-row band). The
//! byte's *value* is what happens there — `FUN_26c0_1e0f` switches on it. Stage 14 read the whole
//! switch and measured every branch against the corpus, which **names** the codes the earlier stages
//! could only describe:
//!
//! | code | meaning (read + measured) | stage |
//! |---|---|---|
//! | 0 (`T_OPEN`) | open ground. The round-start placement accepts **only** this. | 4 |
//! | 1, 0x17 (`T_WATER`) | **water** — the ocean band at the island's edge (level 8) and the margins of the outdoor plates 4 and 6. `DS:0x2F68` rises 2 a frame to **20** and the spy loses **2 cold every 12 frames** | 14 (was "a climb") |
//! | 2 (`T_DEEP_SNOW`) | **deep snow** — level 14's plate. Sink to **6**, `+2` cold every **8** frames, and the whole branch is skipped while the **snowshoes are in hand** (`0x2F2C == 7`) | 14 |
//! | 3 (`T_ICE`) | **ice** — level 12's plate. A moving spy rolls the PRNG; above `0xE6` (9.8 %) he slips: 6 cold, state `0x10`, the other spy laughs | 14 |
//! | 5 (`T_SOLID`) | **solid**: `FUN_26c0_3396` zeroes both velocities, so the commit that follows writes the unchanged position back — the spy does not move | 4 |
//! | 6 (`T_FIRE`) | **the lit fire** — a 4x3 block in the igloo-interior mask (levels 16/22). Solid **and** warming: `+2` heat every **2** frames, so you warm by pressing into it | 14 (was "solid snow") |
//! | 7 (`T_IGLOO_IN`) / 8 (`T_IGLOO_OUT`) | the igloo door: screen ± 10, and the entry/exit x,y | 5 |
//! | 0xC, 0x1D | the igloo interior floor: `+2` heat every **21** frames | 14 |
//! | 0x10, 0x11, 0x14, 0x16, 0x1B, 0x1C, 0x30 | items, furniture, floors — walkable; `FUN_26c0_2de3` (the action key) reads some of them. `0x16` is the one code that leaves the sink word alone (`CS:2014`) | 4/13a |
//!
//! Because the mask's top row (y = 34) and bottom row (y = 58) are **5 everywhere except the door
//! column** (level 2/24 have a four-unit gap at x-offsets 10..13; level 10 at the bottom), the mask
//! *is* the door rule as well: the spy can only reach y = 34/58 inside the gap, and the next step
//! off the band is what `FUN_26c0_0c93`'s caller turns into `screen ± 2`. That is why the level
//! tests `{2,24}`/`{10}` and the mask gap agree on every recorded crossing.
//!
//! Evidence level: **read** (the tables and the switch), with the *effect* measured — see
//! `STAGE4_REPORT.md` §1 for the corpus numbers (positions on solid cells, start positions on
//! code 0, the igloo footprint).

// The module is a library-shaped table: some codes and accessors exist for the report and for the
// item/trap layer the next milestone needs, and are not read by this binary yet.
#![allow(dead_code)]

use crate::round::DS_FILE_OFF;

/// `DS:0x25E0` — one pointer per **even** level value; `word[0x25E0 + level]` (level/2 entries).
pub const DS_MASK_PTR: usize = 0x25E0;
/// `DS:0x25AE` — the x base of each of the 12 world cells: 0, 26, 52, ... 286.
pub const DS_GATE: usize = 0x25AE;
/// `DS:0x24AA` — `word[0x24AA + screen]` = the level row's byte offset (12 words per screen).
pub const DS_ROW_BASE: usize = 0x24AA;
/// `DS:0x24BE` — the level table itself (10 rows of 12 words).
pub const DS_LEVELS: usize = 0x24BE;

/// Bytes per mask row: one per world x unit inside a cell.
pub const MASK_PITCH: usize = 26;
/// Rows per mask: 13, covering y 34..58 two rows at a time.
pub const MASK_ROWS: usize = 13;
/// The y the first mask row stands for (the code's `sub $0x22`).
pub const MASK_Y0: i32 = 34;
/// The highest level value with a mask (the pointer table's 13th entry, level 24).
pub const LEVEL_MAX: u16 = 24;

// ---- code values, named as the switch in FUN_26c0_1e0f treats them -------------------------
/// Open ground. The only code the round-start placement accepts.
pub const T_OPEN: u16 = 0x00;
/// **Water** (stage 14): the sink rises 2 a frame to 20 and the spy loses 2 cold every 12 frames.
/// *Not* solid — a spy can walk into the ocean band and be clipped to the waist in it. Called
/// `T_CLIMB` until stage 14, which is what the mask's effect *looked* like before the heat word
/// was read.
pub const T_CLIMB: u16 = 0x01;
/// The second water code (the margins of the outdoor plates 4 and 6).
pub const T_WATER_HI: u16 = 0x17;
/// **Deep snow** (stage 14): sink to 6, 2 cold every 8 frames, and inert while the snowshoes are in
/// hand. Called `T_SLIP_A` before the branch was read.
pub const T_SLIP_A: u16 = 0x02;
/// **Ice** (stage 14): a 25-in-256 slip roll while moving. `T_SLIP_B` until stage 14.
pub const T_SLIP_B: u16 = 0x03;
/// The same two codes under the names stage 14 measured.
pub const T_WATER: u16 = T_CLIMB;
pub const T_DEEP_SNOW: u16 = T_SLIP_A;
pub const T_ICE: u16 = T_SLIP_B;
/// **The lit fire** (stage 14): solid *and* the fastest warming in the game (a step every 2 frames).
pub const T_FIRE: u16 = 0x06;
/// The igloo interior floor: a warming step every 21 frames.
pub const T_IGLOO_FLOOR_A: u16 = 0x0C;
pub const T_IGLOO_FLOOR: u16 = 0x1D;
/// **Solid.** `FUN_26c0_1e0f` calls `FUN_26c0_3396` (velocities to zero) so the move is refused.
pub const T_SOLID: u16 = 0x05;
/// Solid as well; the switch treats it like `T_SOLID`.
pub const T_SOLID_SNOW: u16 = 0x06;
/// The igloo door: inside the cell, walking onto this spot enters (`screen + 10`).
pub const T_IGLOO_IN: u16 = 0x07;
/// The igloo's way out (`screen - 10`).
pub const T_IGLOO_OUT: u16 = 0x08;
/// The second climb code.
pub const T_CLIMB_HI: u16 = 0x17;
/// **The door arch** (stage 13a): the ice pick refuses unless the spy is standing on this code
/// (`CS:2f41: CMP AX,0xA; JNE return`). It is the mask's pixel value on the level-24 (igloo-door)
/// plate at *even* x offsets — measured: at level 24, x 36 and 38 read 0x0A while x 35 and 31 read
/// 0 — which is why the recorded ice-pick placement happened at an even x in the doorway
/// (`iceaxe` ds_00323) and why the icicle can then land on the door's row.
pub const T_DOOR_ARCH: u16 = 0x0A;
/// The three codes the saw and the bucket accept at the **spy's own** cell (`CS:2e90..2ea4`):
/// open ground, the two furniture/floor codes, and solid. Anything else (a climb, the igloo, ice)
/// refuses the action.
pub const T_CUTTABLE: [u16; 4] = [T_OPEN, T_SOLID, 0x10, 0x11];

/// Item / scenery codes the action key reads; walkable.
pub const T_ITEM: u16 = 0x14;
pub const T_SCENERY: [u16; 5] = [0x0C, 0x1B, 0x1C, 0x1D, 0x30];

/// The original's static world-row x bounds (`DS:0x32B3`/`0x32B7` in the data image: 0 and 304).
pub const X_MIN: i32 = 0;
pub const X_MAX: i32 = 304;

/// The per-level masks, read out of the executable's data segment.
#[derive(Clone, Debug)]
pub struct Terrain {
    /// The 64 KB data segment (`spy.exe` from `0x400`), which is where the mask pointers and the
    /// mask bytes live. Kept whole so a pointer can be followed exactly as the game does.
    ds: Vec<u8>,
}

impl Terrain {
    pub fn from_exe(exe: &[u8]) -> Result<Terrain, String> {
        let ds = exe
            .get(DS_FILE_OFF..DS_FILE_OFF + 0x10000)
            .ok_or_else(|| "spy.exe is too short for a 64 KB data segment".to_string())?
            .to_vec();
        let t = Terrain { ds };
        // every even level up to 24 must have a mask that lies inside the segment
        for level in (0..=LEVEL_MAX).step_by(2) {
            let p = t.mask_ptr(level) as usize;
            if p + MASK_PITCH * MASK_ROWS > t.ds.len() {
                return Err(format!("level {level}'s mask pointer {p:#x} runs off the segment"));
            }
        }
        Ok(t)
    }

    /// `word[0x25E0 + level]` — the mask's address for a level value.
    pub fn mask_ptr(&self, level: u16) -> u16 {
        let a = DS_MASK_PTR + level as usize;
        u16::from_le_bytes([self.ds[a], self.ds[a + 1]])
    }

    /// The x base of a world cell (`0, 26, ... 286`).
    pub fn gate(&self, col: usize) -> i32 {
        let a = DS_GATE + 2 * col.min(11);
        u16::from_le_bytes([self.ds[a], self.ds[a + 1]]) as i32
    }

    /// The column a world x falls in: the last gate at or below x.
    pub fn col_of(&self, x: i32) -> usize {
        let mut col = 0;
        for c in 0..12 {
            if x >= self.gate(c) {
                col = c;
            }
        }
        col
    }

    /// One mask's 338 bytes, for reports and tests.
    pub fn mask(&self, level: u16) -> Option<&[u8]> {
        if level > LEVEL_MAX || level % 2 != 0 {
            return None;
        }
        let p = self.mask_ptr(level) as usize;
        self.ds.get(p..p + MASK_PITCH * MASK_ROWS)
    }

    /// `FUN_26c0_366e`: the behaviour code at (screen, x, y), for **this round's** level table
    /// (the caller passes the engine's, because the round-start icicle flip mutates it).
    ///
    /// `None` when the position is off the tables (odd screen, or a level value with no mask), which
    /// the caller must treat as walkable rather than as a wall — the original would read a garbage
    /// pointer there, and no recorded state does.
    pub fn code(&self, levels: &[[u16; 12]; 10], screen: u16, x: i32, y: i32) -> Option<u16> {
        if screen % 2 != 0 {
            return None;
        }
        let row = (screen / 2) as usize;
        if row >= levels.len() {
            return None;
        }
        let col = self.col_of(x);
        let level = levels[row][col];
        if level > LEVEL_MAX || level % 2 != 0 {
            return None;
        }
        let base = self.mask_ptr(level) as usize;
        // `CS:3698` clamps the row at 0 and nothing else: `y = 32` reads row 0, and `y = 60`
        // reads row **13**, i.e. the 26 bytes *past* the mask (`CS:36b4`'s `mul` is a plain
        // 8x8). When the door passes write `0x32BB`/`0x32BF` the candidate row they probe can be
        // either, so the read has to follow the pointer rather than refuse it.
        let r = ((y - MASK_Y0).max(0) >> 1) as usize;
        let off = x - self.gate(col);
        if off < 0 || off as usize >= MASK_PITCH {
            return None;
        }
        self.ds.get(base + r * MASK_PITCH + off as usize).map(|b| *b as u16)
    }

    /// Is the code one the original refuses to walk onto? `FUN_26c0_1e0f`'s `AX == 5` (rock, wall,
    /// igloo body) and `AX == 6` (**the lit fire**) branches both call `FUN_26c0_3396`, which zeroes
    /// `DS:0x2F00`/`0x2F04` before the caller commits the position — so the spy stays where he is.
    pub fn solid(&self, levels: &[[u16; 12]; 10], screen: u16, x: i32, y: i32) -> bool {
        matches!(self.code(levels, screen, x, y), Some(T_SOLID) | Some(T_SOLID_SNOW))
    }

    /// Is this position one the round start may place a spy on? `FUN_26c0_257b` retries while the
    /// terrain is non-zero, so `T_OPEN` is the acceptance test in the original's own terms.
    pub fn start_ok(&self, levels: &[[u16; 12]; 10], screen: u16, x: i32, y: i32) -> bool {
        self.code(levels, screen, x, y) == Some(T_OPEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> Vec<u8> {
        std::fs::read(crate::orig::require("spy-vs-spy-iii/original/256/spy.exe")).expect("spy.exe")
    }

    fn terrain() -> Terrain {
        Terrain::from_exe(&exe()).expect("terrain tables")
    }

    fn island_one() -> [[u16; 12]; 10] {
        crate::game::IslandData::from_exe(&exe(), 0).expect("island 1").levels
    }

    /// The tables are the executable's: 12 gates 26 apart, 13 rows of 26 bytes per level, and the
    /// two codes the door rule depends on.
    #[test]
    fn mask_tables_are_the_executables_own() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let t = terrain();
        assert_eq!((0..12).map(|c| t.gate(c)).collect::<Vec<_>>(),
                   vec![0, 26, 52, 78, 104, 130, 156, 182, 208, 234, 260, 286]);
        for level in (0..=LEVEL_MAX).step_by(2) {
            assert_eq!(t.mask(level).map(|m| m.len()), Some(MASK_PITCH * MASK_ROWS),
                       "level {level}");
        }
        // level 20 (the igloo interior floor) shares level 0's mask
        assert_eq!(t.mask_ptr(20), t.mask_ptr(0));
        // the level-2 up door has a four-unit gap at x-offsets 10..13 of its top row
        let m = t.mask(2).unwrap();
        assert_eq!(&m[0..14], &[5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 0, 0, 0, 0]);
        assert_eq!(m[14], 5);
        // level 10's gap is on the bottom row
        let m = t.mask(10).unwrap();
        assert_eq!(&m[12 * MASK_PITCH + 10..12 * MASK_PITCH + 14], &[0, 0, 0, 0]);
        assert_eq!(m[12 * MASK_PITCH + 9], 5);
    }

    /// The wall that stops a spy walking out of the band: every level's top and bottom rows are
    /// solid away from the door gap, and level 8's middle band is *not* solid (it is `T_CLIMB`).
    #[test]
    fn the_band_walls_are_solid_and_the_void_is_not() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let t = terrain();
        let lv = island_one();
        // island 1 room 4 is [8,8,6,0,0,2,16,4,8,8,8,8]
        assert_eq!(lv[2], [8, 8, 6, 0, 0, 2, 16, 4, 8, 8, 8, 8]);
        // y = 34 is solid over the open-ground cell (col 3) but open inside the door (col 5)
        assert!(t.solid(&lv, 4, 26 * 3 + 5, 34));
        assert!(!t.solid(&lv, 4, 26 * 5 + 11, 34));
        // y = 58 is solid under the open-ground cell (the down door is on screen 6)
        assert!(t.solid(&lv, 4, 26 * 3 + 5, 58));
        // the rock (level 6, col 2) is solid in its body (its mask's left third) and the void
        // (level 8, col 1) is not solid at all
        assert!(t.solid(&lv, 4, 26 * 2 + 3, 44));
        assert!(!t.solid(&lv, 4, 26 * 2 + 12, 44));
        assert_eq!(t.code(&lv, 4, 26 * 1 + 10, 44), Some(T_CLIMB));
        // the open snowfield
        assert_eq!(t.code(&lv, 4, 26 * 3 + 10, 44), Some(T_OPEN));
    }

    /// The igloo footprint: level 16 is solid over its body and open only in the door alcove, which
    /// is where every recorded igloo entry happened (x-offsets 21..22, y 40..44).
    #[test]
    fn the_igloo_is_solid_except_its_door_alcove() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let t = terrain();
        let lv = island_one();
        let col = 6; // island 1 room 4: level 16 at column 6
        let base = 26 * col;
        // the igloo's wall (y 34..38) and its base (y 44..48)
        for y in [36, 38] {
            for off in [2, 5, 9, 14] {
                assert!(t.solid(&lv, 4, base + off, y),
                        "igloo wall at x-offset {off}, y {y} must be solid");
            }
        }
        for off in [3, 8, 14, 19] {
            assert!(t.solid(&lv, 4, base + off, 44),
                    "igloo base at x-offset {off}, y 44 must be solid");
        }
        // the alcove the spies actually used
        for y in [40, 42, 44] {
            for off in [21, 22] {
                assert!(!t.solid(&lv, 4, base + off, y),
                        "the igloo door at offset {off}, y {y} must be walkable");
            }
        }
    }

    /// The round-start acceptance test: on island 1 the ocean west of the room (col 1, level 8) is
    /// rejected while the snowfield (col 3, level 0) is accepted — the port's old start, x = 29.
    #[test]
    fn the_start_test_rejects_the_ocean_and_accepts_the_snowfield() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let t = terrain();
        let lv = island_one();
        assert_eq!(t.code(&lv, 4, 29, 44), Some(T_CLIMB));
        assert!(!t.start_ok(&lv, 4, 29, 44));
        assert!(t.start_ok(&lv, 4, 26 * 3 + 12, 44));
    }
}
