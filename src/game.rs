//! The round as a game: a deterministic engine (fixed 60 Hz tick, no wall clock) plus the drawing
//! of what it produces through `round.rs`'s viewport composer.
//!
//! Everything the engine moves is a number the original keeps in its data segment, so the engine
//! is written in the original's own units:
//!
//! * `x` is a **byte column of the 312-byte world row** (12 cells x 26 bytes; one byte is four
//!   screen pixels, so the 44-byte viewport is the 176-px window measured in stage 1). Evidence:
//!   the round loop clamps `DS:0x24A6` (the scroll) to `0x10C` = 312 - 44, and the door standoff
//!   `gate[col] + 0x0B` is what the measured door column `x ~ 141` works out to for `col 5`.
//! * `y` is the spy's foot row inside the 58-row viewport, measured constant at **34..58** over
//!   every dump the rig recorded (18,915 of them, and 200k+ sampled for this stage).
//! * the screen id is `DS:0x329F` (+2 for player 2): even, and **bigger means further up** — the
//!   measured room changes are `screen + 2` for an up door and `screen - 2` for a down one.
//!
//! Nothing here reads the clock: `tick()` is the only way time passes, and the scripted
//! (`--script`) and windowed paths both feed it the same `Input`. That keeps the port a pure
//! function of (island, seed, input prefix), which is the contract in ADR 0002.
//!
//! ## Evidence levels, per constant
//!
//! Every constant below carries a `measured`, `read` (from the disassembly) or `inferred` label.
//! The ones that are guesses are also runtime flags where that costs nothing, because the workspace
//! rule is "prefer a flag to a silent guess".

// The engine is a library-shaped module: some accessors and constants exist for the report and for
// the next milestone (objects, traps, heat) and are not read by this binary yet.
#![allow(dead_code)]

use std::path::Path;

use crate::cpu;
use crate::sound;
use crate::chrome;
use crate::items::{self, CARRY_NONE};
pub const CARRY_EMPTY: u16 = 0xFFFF;
use crate::round::{self, Art, RoundState, STRIP_LEN, STRIP_PITCH, STRIP_ROWS};
use crate::terrain::{self, Terrain};
use crate::Screen;

// ---- time -----------------------------------------------------------------
// The frame-sync loop waits on a **timer word**, and the timer is not the BIOS' 18.2 Hz one. Stage
// 16a read the whole chain out of the executable; the numbers below are the executable's own:
//
//   4574  mov al,0x36 ; out 0x43        ; PIT channel 0, mode 3, lobyte/hibyte, binary
//   4578  mov ax,0x136C ; out 0x40 (x2) ; divisor 4972
//   4562  int 21h AH=25, DX=0x4196      ; INT 1C -> the game's own tick handler
//   4196  inc word[0x33F4] ; inc word[0x33F6] ; ... the two counter words
//   063c  and word[0x33F4],0x7f ; cmp 0x1a ; jl 0x63c     ; THE FRAME WAIT
//   0658  mov word[0x33F4],0                               ; zeroed after the wait
//   0680  cmp word[0x33F6],0xdc ; jle skip                 ; THE CLOCK, tested once a frame
//
/// **read** — the engine's tick is the original's timer interrupt: PIT channel 0 reprogrammed to
/// divisor 0x136C (4972), i.e. 1193181.8182 / 4972 = **239.98 Hz**, one tick every 4.167 ms. (The
/// BIOS' 18.2 Hz is gone the moment `CS:4574` runs; the game chains INT 1C, not INT 08.)
pub const TICK_HZ: u32 = 240;
/// **read + measured** — one game frame is **26 ticks**. `CS:063c..0647` waits for the low seven
/// bits of `DS:0x33F4` to reach 0x1A, and `CS:0658` zeroes that word at the top of every frame, so
/// the wait is 26 ticks from zero: **108.34 ms**, 9.23 frames a second.
///
/// It is a **minimum**, not a deadline: the zeroing happens *after* the wait and the body follows,
/// so a frame whose logic and drawing outlast 26 ticks simply falls through the next wait and runs
/// long — by whole ticks (4.17 ms), since that is the word's granularity. The word is masked to
/// seven bits, so a body over 128 ticks would also wrap (533 ms; no recording shows one).
/// Measured: 128 of 213 recorded 138-frame cycles at 108.21..108.45 ms, the rest 109.9..117.9
/// (emulator host-time lag), never below 108.2 — see `STAGE16A_REPORT.md` §1-§2.
pub const FRAME_TICKS: u32 = 0x1A;
/// 26 ticks at 239.98 Hz: 26 x 4972 / 1193181.8182 = **108.342 ms**. (Two BIOS ticks would be
/// 109.85 ms — the task's first estimate; the reprogrammed divisor is what the machine ran, and the
/// recordings agree with 108.3, not with 109.9.)
pub const FRAME_MS: f64 = 108.342;
/// Frames a second: 9.2298. Kept as a float because it is not a round number — the original's frame
/// is 26 timer ticks and **not** 10 Hz (`STAGE4_REPORT.md`'s "10 Hz" reading was 6 of the port's own
/// 60 Hz ticks, which is 100 ms, not the machine's 108.3).
pub const FRAME_HZ: f64 = 1000.0 / FRAME_MS;
/// `DS:0x33F6`'s threshold: `CS:0680` compares it with 0xDC (220) **once per frame** and clears it
/// when it is above. 8 frames are 208 ticks and never reach it; 9 are 234 and always do — so one
/// displayed clock second is **9 frames = 975.1 ms** and the original's clock runs 2.5 % fast
/// against a wall-clock second. **read** (`CS:065e`..`CS:068e`) + **measured** (975.0 ms a count
/// over 2,155 counts in five sessions, `STAGE16A_REPORT.md` §2.3).
pub const CLOCK_TICKS_WORD: u16 = 0xDC;
/// Frames per clock count, derived from the two words above (not a separate constant in the image).
pub const CLOCK_FRAMES: u32 = 9;
/// Position resolution: thirds of a world unit. The engine keeps positions this way so a
/// half-frame never rounds, but every frame moves a whole unit — as the original does.
pub const SUB: i32 = 3;

// ---- movement -------------------------------------------------------------
/// **read** — the four direction setters write the velocity words directly: `FUN_26c0_32b4` (left)
/// and `FUN_26c0_32ed` (right) set `DS:0x2F04` to ±1, `FUN_26c0_3326` (up) and `FUN_26c0_335e`
/// (down) set `DS:0x2F00` to ∓2. One unit is 4 screen px.
pub const WALK_X_PER_FRAME: i32 = 1;
pub const WALK_Y_PER_FRAME: i32 = 2;
/// **measured** — the speed you get while the viewport scrolls: 2 units a frame. The frame is
/// 108.34 ms (stage 16a), so that is **18.5 units a second of wall clock** — the stage-4 report's
/// "20 units/s" came from 10 frames a second, which was 6 of the port's own 60 Hz ticks — and
/// exactly **18 units per clock second** (9 frames, `CLOCK_FRAMES`).
pub const WALK_UNITS_PER_SEC: i32 = 18;

/// **read** — the walkable band's vertical bounds are a static pair in the data image
/// (`DS:0x32BB` = 34, `DS:0x32BF` = 58). `y` leaving it is what a door step tests.
pub const Y_MIN: i32 = 34;
pub const Y_MAX: i32 = 58;

// ---- the walk animation ---------------------------------------------------
/// **read** — `DS:0x2F10`, the frame ceiling each direction setter writes.
pub const ANIM_MAX: u16 = 4;
/// **read** — `DS:0x2F08`, the frame step each direction setter writes (±2).
pub const ANIM_STEP: i16 = 2;
/// **read** — `FUN_26c0_3396`, which runs whenever no direction is held, sets `DS:0x2EFC` to 2.
/// That is the standing frame, and it is also the "both legs down" frame in the walk cycle.
pub const STAND_FRAME: u16 = 2;

// ---- the two spies' bodies (stage 9b) -------------------------------------
/// **read** — `DS:0x2F1C` (+2), the vertical tolerance of the contact test at `CS:3624`/`3634`.
/// The data image holds **1** for both spies, and `y` steps by 2, so this is exactly "the same
/// row": two spies one row apart can share an x (measured: 3 recorded dumps with dx = 0, dy = 2).
pub const SPY_CONTACT_Y: i32 = 1;
/// **read** — `DS:0x2F20` (+2), the horizontal half-width at `CS:3644`/`3656`. **6** in the image
/// for both spies. It is zeroed by `FUN_26c0_2238` (`CS:2269`/`226f`, the freeze), which is why a
/// spy who is out of the round does not block the other one.
pub const SPY_CONTACT_X: i32 = 6;
/// **read** — the `add $3` (`CS:3648`) and `sub $3` (`CS:365b`) that narrow the x window:
/// collision when `other.x - 3 <= my tentative x <= other.x + 3`. With a 1-unit x step and a
/// 2-unit scroll step, the closest two spies can stand on one row is therefore **4** — which is
/// what the recordings show (5 dumps at dx = 4, none below).
pub const SPY_CONTACT_INSET: i32 = 3;
/// The frames the counter bounces through: 0, 2, 4 and back through 2. `FUN_26c0_0c93`'s tail
/// (`CS:0a06`) is `if (frame + step < 0 || frame + step > 0x2F10) step = -step; frame += step;`,
/// so the sequence is the triangle **0, 2, 4, 2** — which is why 2 is the most common value in the
/// recordings (52%, against 25% and 22% for 4 and 0).
pub const WALK_FRAMES: [u16; 4] = [0, 2, 4, 2];

// ---- doors ----------------------------------------------------------------
/// One world cell, in bytes of the strip (= 4 screen px).
pub const CELL: i32 = 26;
/// **read + measured** — the level word (`word[0x24BE + 12*screen + 2*col]`) says what is in the
/// wall: 2 or 24 = an up door, 10 = a down door, 16 = an igloo door.
pub const LEVEL_DOOR_UP: [u16; 2] = [2, 24];
pub const LEVEL_DOOR_DOWN: u16 = 10;
pub const LEVEL_DOOR_IGLOO: u16 = 16;
/// **measured** — the spy passes only with `x` in `26*col + 10 ..= 26*col + 13` (checked against
/// 80 of 80 recorded crossings). The disassembly compares against `gate[col] + 0x0B` = `26*col+11`,
/// which is the centre of that window.
pub const DOOR_CX_MIN: i32 = 10;
pub const DOOR_CX_MAX: i32 = 13;
/// **read + measured** — the igloo door is the mask's own `T_IGLOO_IN` (code 7) cell, and *walking
/// onto it* is the whole trigger: `FUN_26c0_1e0f`'s `AX == 7` branch sets
/// `screen += 10`, `x = colbase + 0xF`, `y = 0x30`, `map_x = colbase - 8`, and the caller's commit
/// then adds the frame's own velocity. No key is involved (`FUN_26c0_2de3`, the action key, has no
/// igloo branch at all — it is items and traps only). Every recorded entry passes through the
/// alcove at `cx` 21..22 with `y` 40..44, which is exactly the cell *in front of* the code-7 cell,
/// and every recorded landing is `colbase + 14, y 48` = `+0xF` then one unit west, with the view at
/// `colbase - 8` (36 of 36 entries, see `STAGE5_REPORT.md`).
///
/// `IGLOO_ENTER_CX`/`_Y` keep the stage-4 reading (the action key at the alcove) as the
/// `--igloo-entry=key` fallback; they are the coordinates a spy occupies on the frame *before* the
/// walk-in fires, which is why the recordings looked like a key window.
pub const IGLOO_ENTER_CX: (i32, i32) = (21, 22);
pub const IGLOO_ENTER_Y: (i32, i32) = (40, 44);
/// **read** — the landing inside the igloo: `x = colbase + 0xF` (the commit then adds the frame's
/// velocity, so a spy walking west lands on `colbase + 14`), `y = 0x30` = 48, `map_x = colbase - 8`.
pub const IGLOO_ENTRY_X_OFF: i32 = 0x0F;
pub const IGLOO_ENTRY_Y: i32 = 0x30;
pub const IGLOO_ENTRY_MAP_OFF: i32 = -8;
/// The exit: the interior mask's `T_IGLOO_OUT` (code 8) at `cx` 17 (`FUN_26c0_1e0f`'s `AX == 8`
/// puts the spy at `colbase + 0x16, y = 0x2A`); every recorded exit was taken from `cx` 16, one
/// step short of the cell, because the transition fires inside the move.
pub const IGLOO_EXIT_CX: i32 = 16;
pub const IGLOO_EXIT_Y: (i32, i32) = (46, 48);
/// Igloo interiors are `screen + 10` (`4 -> 14` measured by walking into one), so any screen id at
/// or above this is an interior and has no level row in the 5x12 table.
pub const FIRST_INTERIOR_SCREEN: u16 = 10;

/// **inferred** — where a spy stands after a vertical door step. The original restores `y` from a
/// per-screen saved value (`DS:0x32BF`, `0x32BB`), which this port does not model; entering from
/// below puts the spy on the new room's bottom row and vice versa. One keypress to notice if wrong.
pub const DOOR_ENTRY_FROM_BELOW_Y: i32 = Y_MAX;
pub const DOOR_ENTRY_FROM_ABOVE_Y: i32 = Y_MIN;

// ---- the scroll window ----------------------------------------------------
/// The viewport is 44 strip bytes wide and the world row 312, so the scroll clamps to 312 - 44.
pub const MAP_X_MAX: i32 = 0x10C;
/// **read** — the spy sits between 6 and 30 units from the window's left edge: the dead zone in
/// `FUN_26c0_0000` is `x - 6 < mapX` (scroll left) and `x > mapX + 0x1E` (scroll right).
pub const SCROLL_LEFT_EDGE: i32 = 6;
pub const SCROLL_RIGHT_EDGE: i32 = 30;
/// **read** — the scroll step is 2 units per frame (`*(int *)0x2F82` = +-2).
pub const SCROLL_STEP: i32 = 2;
/// **read** — on a room change the scroll is set to `x - 0x14` (the round-start code does the same),
/// then clamped.
pub const MAP_X_ON_ENTRY_OFFSET: i32 = 20;

// ---- the shared-screen visitor window -------------------------------------
/// **measured + read** — the "visitor" window: `FUN_26c0_0000` (`CS:0111`) sets `DS:0x2F7E` for a
/// spy on the *same screen* as the other when the other's scroll is within
/// `map_x - 4 < x < map_x + 0x29` (45 world units, 180 px). It *clears* the flag when the spy
/// walks out of that window and then gives him a scroll of his own, `map_x ± 0x21` (33 units).
/// A room *step* (`CS:08ef`) uses the wider entry test `map_x <= x <= map_x + 0x2C`, because right
/// after a door the newcomer is somewhere in the 44-unit view.
pub const VISITOR_ENTRY_LEFT: i32 = -4;
pub const VISITOR_ENTRY_RIGHT: i32 = 0x29;
pub const VISITOR_STEP_RIGHT: i32 = 0x2C;
/// **read** — the gap the visitor's own scroll is put at when the flag clears (`±0x21`).
pub const VISITOR_KEEP_OFFSET: i32 = 0x21;
/// **read** — `CS:0887`: a visitor who leaves a shared room through a door gets his own scroll as
/// `x - 0x16`, i.e. 22 units in from the left edge of his new room — not the host's scroll he was
/// carrying (which is what made his console show the wrong column).
pub const VISITOR_OWN_MAP_OFFSET: i32 = 0x16;
/// **read** — the two guards on a *visitor* pushing the host's frame (`FUN_26c0_0000`, `CS:00e2`
/// and `CS:010d`). Both keep the **host** inside his own dead zone minus one step of clearance, so
/// the push stops one step short of the point where the host would start scrolling himself:
///
/// ```text
/// east (map += 2):  if (host.x - 8 < host.map)   no scroll   -> host stops at screen x 6
/// west (map -= 2):  if (host.map + 0x1C < host.x) no scroll  -> host stops at screen x 30
/// ```
///
/// The asymmetry is the code's (8 = `0x06 + 2`, 0x1C = `0x1E - 2`), not a reading error: both are
/// the dead zone's edge plus or minus the 2-unit step, and the recordings land exactly on 6 and 30
/// (`item-check-2` 00335.. and `trapulator-island4` 02015..02033).
pub const VISITOR_PUSH_EAST_MIN: i32 = 8;
pub const VISITOR_PUSH_WEST_MAX: i32 = 0x1C;
/// **read** — the visitor's own two bounds on pushing: east only while `x < host.map + 0x22`
/// (`CS:00cf`), west only while `x - 4 >= host.map` (`CS:00fd`).
pub const VISITOR_PUSH_SELF_EAST: i32 = 0x22;
pub const VISITOR_PUSH_SELF_WEST: i32 = 4;

// ---- stage 16b: the snowball (`FUN_26c0_2f87`, `CS:303c`, `CS:30c9`) ----------------------
//
// Everything below is read out of the executable; the measured corpus (46 throws, 48 knock-outs in
// four sessions) agrees with every one of them (STAGE16B_REPORT.md §2).
/// **read** — `CS:3021..3028`: the ball's row is the thrower's foot row minus `0x12` (18), fixed for
/// the whole flight. There is no gravity and no arc: `0x32EF` is loaded once, when the throw starts.
pub const BALL_ROW_FROM_FEET: i32 = 0x12;
/// **read** — `CS:304e`: a ball whose row is already past `0x35` (53) is not flown at all.
pub const BALL_ROW_MAX: i32 = 0x35;
/// **read** — `CS:2fd4..2fdf`: an **eastward** throw starts six bytes ahead of the spy and moves +3;
/// a **westward** one (`CS:3001..3009`) starts on the spy's own column and moves -3. That
/// asymmetry is the code's, and it is why a spy at the thrower's own column can never be hit.
///
/// **stage 21a** — it is also why the owner's point-blank report is a property of the original and
/// not a defect here: `CS:3046` steps the column *before* `CS:307c` tests it, so the first column an
/// eastward ball is ever tested at is `x + 9`, while a victim standing nose to nose (`|dx| = 4`, the
/// blocking contact distance) has his box at `[x + 5, x + 8]`. The eastward throw at contact range
/// misses **for ever** — the ball is born past the far edge — and the westward one hits at `x - 3`.
/// Recorded: `playtest5` `ds_00660` is a westward knock-out at `dx = -4` (launch 216, hit column
/// 213); no eastward one in the corpus is closer than `dx = +6` (`ds_00821`). STAGE21A_REPORT.md.
pub const BALL_EAST_LEAD: i32 = 6;
/// **read** — `0x32FB`: three world bytes (12 pixels) a frame.
pub const BALL_SPEED: i32 = 3;
/// **read** — `CS:09b1`: the ball is released when the throw's frame word reaches 8, i.e. on the
/// fifth of the eight drawn frames.
pub const BALL_LAUNCH_FRAME: i16 = 8;
/// **read** — `CS:3056..3077`: the ball is still in the air while
/// `view - 2 <= col < view + 0x2C` in the *host's* window (`view` is the thrower's own scroll, or
/// the other spy's when the thrower is the visitor).
pub const BALL_VIEW_MARGIN: i32 = 2;
pub const BALL_VIEW_WIDTH: i32 = 0x2C;
/// **read** — `CS:3080..30a5`: the hit box, in world bytes and rows, against the *other* spy:
/// `his_x + 1 <= col <= his_x + 4`, `his_y - 0x18 <= row <= his_y - 0x0E`.
pub const BALL_HIT_X: [i32; 2] = [1, 4];
pub const BALL_HIT_Y: [i32; 2] = [0x18, 0x0E];
/// **read** — `CS:2fab..2fbe`: the four terrain codes that refuse the throw. Climbing and high
/// water refuse every directed action; deep snow and ice are the snowball's own two.
pub const BALL_REFUSED_TERRAIN: [u16; 4] = [1, 2, 3, 0x17];
/// **read** — the sound driver's cue for the hit (`CS:30dc`) and the id (`CS:30ee`, `-2`). Recorded
/// only; nothing is emitted.
pub const BALL_CUE: u16 = 0x9F60;
pub const BALL_SOUND: i16 = -2;

// ---- stage 19a: the rocket's window (`CS:2865..2877`) --------------------------------------
/// **read** — `DS:0x2E06` (+2 per player): the table of pointers `CS:286f` loads
/// (`mov 0x2e06(%si),%si`). It holds `0x3C32` for player 1 and `0x3C93` for player 2 in the
/// executable's own data segment and in every recorded dump: two canned `[19][5]` pictures, the
/// white spy and the black spy.
pub const DS_STAGED_SPRITE: usize = 0x2E06;
/// **read** — `CS:2868`: the copy's bottom row, `0x32` (50) — the same frame the descent's feet use
/// (`0x36` is the room's floor row, `0x32` the sprite's own bottom inside it).
pub const ROCKET_WINDOW_BOTTOM: i32 = 0x32;
/// **read** — `CS:286b`: the copy's column, `2`, inside the record's 9-byte row.
pub const ROCKET_WINDOW_COL: usize = 2;


// ---- round start ----------------------------------------------------------
/// **read** — the round-start placement (`FUN_26c0_257b`) draws `y = 0x2C` = 44,
/// `x = 26 + (rand & 0xFF)` and `screen = rand & 6`, and **retries until the terrain code at that
/// spot is exactly 0** (`if (FUN_26c0_366e() == 0)`). The second spy is also required to draw a
/// different screen from the first (`if ((rand & 6) == word[0x32A1]) retry`). Every landed start in
/// the corpus satisfies the test; see `STAGE4_REPORT.md` §1.
pub const START_Y: i32 = 44;
pub const START_X_BASE: i32 = 26;
/// The two spies are placed in one loop, player 2 first; this is the `rand & 6` mask on the screen.
pub const START_SCREEN_MASK: u32 = 6;
/// **read** — the pose the placement writes after a successful draw: the plain walk set (0x24) and
/// direction 4 (`piVar12[0x1786] = 4`) with `mapX = x - 0x14`.
pub const START_DIR: u16 = 4;

/// DS image = `spy.exe` from file 0x400 (the load base is paragraph 0x1000, DS is 0x1020).
pub const DS_FILE_OFF: usize = 0x400;

// ---- the per-island record in spy.exe's data segment ----------------------
/// **read** — `DS:0x72C0` is eight 7-entry word arrays, stride 14 bytes, indexed by the island
/// (`DS:0x31BB / 2`, 0..6). The round-start loader (`FUN_26c0_2549`'s caller) reads:
///
/// ```text
///   +0x00  object block pointer (0x3E0 bytes)      -> copied to DS:0x1052
///   +0x0E  unknown (0x1A, 0x26, 0x3A, 0x42 ...)
///   +0x1C  level table pointer (120 words)         -> copied to DS:0x24BE   <- used here
///   +0x2A  round clock, seconds                    -> DS:0x324D             <- used here (measured)
///   +0x38  player 1 start screen                   -> DS:0x329F             <- used here
///   +0x46  player 2 start screen                   -> DS:0x32A1
///   +0x54  starting kit word                       -> DS:0x32A3/0x32A5
///   +0x62  unknown (0x0A, 0x0A, 0x0A, 8, 6, 4, 2)
/// ```
///
/// The clock column comes out as 360/480/660/840/1080/1320/1620 s for islands 1-7, which is exactly
/// the per-island clock the rig measured independently — so the table is the right one.
pub const DS_ISLAND_TABLE: usize = 0x72C0;
pub const ISLAND_STRIDE: usize = 14;

/// One island's round-start data, as the original loads it.
#[derive(Clone, Debug)]
pub struct IslandData {
    pub island: usize,
    pub levels: [[u16; 12]; 10],
    pub clock_secs: u16,
    pub start_screen_p1: u16,
    pub start_screen_p2: u16,
    pub kit: u16,
    pub object_block: u16,
    pub objects_unknown: u16,
    pub thermal_unknown: u16,
}

fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

impl IslandData {
    pub fn from_exe(exe: &[u8], island: usize) -> Result<IslandData, String> {
        if island > 6 {
            return Err(format!("island {island} out of range 0..6"));
        }
        let ds = exe
            .get(DS_FILE_OFF..DS_FILE_OFF + 0x10000)
            .ok_or_else(|| format!("spy.exe is {} bytes, too short for a 64 KB data segment", exe.len()))?;
        let field = |k: usize| -> usize { DS_ISLAND_TABLE + k * ISLAND_STRIDE + 2 * island };
        let levels_ptr = u16le(ds, field(2)) as usize;
        let clock_secs = u16le(ds, field(3));
        let start_screen_p1 = u16le(ds, field(4));
        let start_screen_p2 = u16le(ds, field(5));
        let kit = u16le(ds, field(6));
        let object_block = u16le(ds, field(0));
        let objects_unknown = u16le(ds, field(1));
        let thermal_unknown = u16le(ds, field(7));
        if levels_ptr + 240 > ds.len() {
            return Err(format!("island {}'s level table pointer {levels_ptr:#x} runs off the segment", island + 1));
        }
        let mut levels = [[0u16; 12]; 10];
        for (r, row) in levels.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                *v = u16le(ds, levels_ptr + 24 * r + 2 * c);
            }
        }
        Ok(IslandData {
            island,
            levels,
            clock_secs,
            start_screen_p1,
            start_screen_p2,
            kit,
            object_block,
            objects_unknown,
            thermal_unknown,
        })
    }

    /// The first row index that holds a room with at least one door — used only when
    /// `--random-start` would otherwise drop a spy on an island's door-less filler row.
    pub fn first_playable_row(&self) -> usize {
        for (r, row) in self.levels.iter().enumerate() {
            if row.iter().any(|&v| {
                LEVEL_DOOR_UP.contains(&v) || v == LEVEL_DOOR_DOWN || v == LEVEL_DOOR_IGLOO
            }) {
                return r;
            }
        }
        0
    }
}

// ---- input ---------------------------------------------------------------
/// One tick's worth of input for one player. The original reads the keyboard into a per-player
/// word-flag set (`0x3287` up, `0x328b` down, `0x328f` left, `0x3293` right, read off the
/// disassembly) plus **two** fire flags: button A at `0x327f` and button B at `0x3283`.
///
/// The port has a key for A only — `fire` — because the owner has one fire key (stage 7's
/// deviation 1: a tap is A, fire held plus a direction is B). `fire_b` is the *second* button,
/// and only the computer presses it: `CS:129c`'s attack sets `0x3283` directly (`CS:1a46`), and
/// nothing in the port's keyboard mapping ever fills it in. The distinction is not cosmetic —
/// A is consumed by acting (`0x2d75` clears the flag, and `0x2F28` latches the act), while B is
/// tested as a level on every frame the frame body runs (`CS:0791` → `0x2d7e`), so an AI that
/// holds B re-presses it each frame and a player who holds A acts once. See STAGE18F_REPORT.md.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keys {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub fire: bool,
    /// `DS:0x3283` — button B. The computer's punch; never set by the keyboard.
    pub fire_b: bool,
}

impl Keys {
    pub fn any(&self) -> bool {
        self.up || self.down || self.left || self.right || self.fire || self.fire_b
    }

    /// The union of two samples, for the frame latch: a key that was down for **any** tick of a
    /// frame counts as down for that frame.
    pub fn or(self, other: Keys) -> Keys {
        Keys {
            up: self.up || other.up,
            down: self.down || other.down,
            left: self.left || other.left,
            right: self.right || other.right,
            fire: self.fire || other.fire,
            fire_b: self.fire_b || other.fire_b,
        }
    }
}

/// Both players' input for one tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub p1: Keys,
    pub p2: Keys,
}

/// **Stage 24a** — one recorded demo mask (`DEMO.DAT`) as the six key flags of each spy.
///
/// `CS:0203`'s twelve tests, in the order the disassembly does them:
///
/// | bit | word | key | bit | word | key |
/// |---|---|---|---|---|---|
/// | 0 | `0x3287` | P1 up | 8 | `0x3289` | P2 up |
/// | 1 | `0x3293` | P1 right | 9 | `0x3295` | P2 right |
/// | 2 | `0x328B` | P1 down | 10 | `0x328D` | P2 down |
/// | 3 | `0x328F` | P1 left | 11 | `0x3291` | P2 left |
/// | 4 | `0x327F` | P1 A (`fire`) | 12 | `0x3281` | P2 A |
/// | 5 | `0x3283` | P1 B (`fire_b`) | 13 | `0x3285` | P2 B |
///
/// The six words per spy run off the pointer table at `0x327F + si` in the order A, B, up, down,
/// left, right, which is why bit 0 is `up` and not `A`. Confirmed against the recording: at
/// `playtest3` ds_00247 (demo frame 7, mask `0x0004`) the dump's P1 key words read
/// `[0,0,0,1,0,0]` — index 3, `0x328B`, down — and the mask's bit 2 is the only bit set.
pub fn demo_keys(mask: u16) -> (Keys, Keys) {
    fn six(bits: u16) -> Keys {
        Keys {
            up: bits & 0x01 != 0,
            right: bits & 0x02 != 0,
            down: bits & 0x04 != 0,
            left: bits & 0x08 != 0,
            fire: bits & 0x10 != 0,
            fire_b: bits & 0x20 != 0,
        }
    }
    (six(mask & 0x3F), six((mask >> 8) & 0x3F))
}

/// What the round is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The clock is running and the spies can move.
    Playing,
    /// The round is over. `Game::end` says which of the original's three exits fired.
    Over,
}

/// Why the round ended — the three exits the outer loop has:
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundEnd {
    /// `CS:065e` — `DS:0x324D` reached 0 and nothing was left animating or flying. The measured
    /// behaviour is a straight cut to the options screen with no winner and no score.
    Clock,
    /// `CS:0649` — the ascent finished (`DS:0x248A`), i.e. somebody launched.
    Launch,
    /// `CS:06c0` — both spies are out of the round (`0x2F64` and `0x2F66` both >= 2): both frozen.
    BothOut,
}

/// One spy's state, in the original's units.
#[derive(Clone, Debug)]
pub struct SpyState {
    /// World byte column, in `SUB`ths of a unit (x3).
    pub x3: i32,
    /// Foot row, in `SUB`ths of a unit.
    pub y3: i32,
    /// `DS:0x329F` (+2 for player 2).
    pub screen: u16,
    /// `DS:0x2F0C`: 0 = horizontal, 2 = up, 4 = down (read off the direction setters). This is the
    /// *direction*, not the pose set — the two are separate words in the original and conflating
    /// them (`0x24` written here) makes `word[0x2622 + 0x24]` select a garbage legs row.
    pub dir: u16,
    /// `DS:0x2EFC` — the walk-cycle phase as the original keeps it: **0, 2 or 4**, and the counter
    /// bounces, so the sequence is 0, 2, 4, 2 (see [`WALK_FRAMES`]). 2 is also the standing frame.
    pub frame: u16,
    /// `DS:0x2F08` — the frame's step: ±2 while walking, 0 while standing. `FUN_26c0_3396` zeroes
    /// it when no key is held, which is what makes the legs settle on frame 2.
    pub frame_step: i16,
    /// The velocity words, in whole world units per frame: `DS:0x2F04` (x) and `DS:0x2F00` (y).
    pub vel_x: i32,
    pub vel_y: i32,
    /// `DS:0x2EF8` — the pose *set*; 0x24 (36) is the plain walk and is what every recorded round
    /// start holds.
    pub set: u16,
    /// `DS:0x32AB` (+2) — an extra byte offset into the record table: 0 for player 1 and 0x2EC for
    /// player 2, measured constant in every dump.
    pub extra: u16,
    /// `DS:0x2F2C` == 7: shifts the legs row by 0x12. Default false; the verification path takes it
    /// from the dump. Nothing in a live round sets it yet.
    pub pose_row_shift: bool,
    /// `DS:0x2F18` — the raised (climb / enter) branch. Default false, unmodelled in play.
    pub pose_raised: bool,
    /// `DS:0x32CF` — the pose's own sub-state; 0x0C and 0x14 arm the vertical clip. Default 0.
    pub pose_state: u16,
    /// `DS:0x2F68` — the vertical clip offset (raised branch). Default 0.
    pub pose_clip: u16,
    /// `DS:0x32A7` — the pose set an animation saved, restored when it ends (`CS:27a8`/`CS:280d`).
    pub saved_set: u16,
    /// `DS:0x32AF` — the direction restored by the same prologue (`CS:281b`). The word is never
    /// written inside this segment, so it holds the round-start value (0) for a whole round, which
    /// is why a spy always comes out of an animation facing horizontally.
    pub saved_dir: u16,
    /// **port state for `DS:0x2F70`** — the rocket is in flight: `0xc93` draws the rocket instead
    /// of the spy, so this spy is not drawn at all. Set by the launch branch (`CS:28ae`).
    pub launched: bool,
    /// **port state for `DS:0x2F64 == 2`** — out of the round: `FUN_26c0_0000` returns at its first
    /// test, so this spy neither moves nor scrolls, and `0xc93` skips him (`CS:cc0`). Set on the
    /// other spy by the launch (`CS:28ba`).
    pub out_of_round: bool,
    /// `DS:0x2F14`: 1 = facing west.
    pub mirror: bool,
    /// `DS:0x24A6` (+2): the scroll window's left edge, in world units.
    pub map_x: i32,
    /// The console is blanked to flat cyan (the shared-screen visitor rule, `DS:0x2F7E`).
    pub blank: bool,
    pub fire_prev: bool,

    // ---- the items (stage 7) ---------------------------------------------------------------
    /// `DS:0x2F2C` (+2) — the carried item code: `0xFFFF` = nothing, else 1..12. **measured**
    /// (the word is `-1` / 7 / 9 / 10 / 11 / 12 across the corpus) + **read** (`FUN_26c0_2d22`).
    pub carried: u16,
    /// `DS:0x32A3` (+2) — the kit word: the seven tool bits {0,1,2,3,4,5,11} = that tool is in the
    /// trapulator, bits 6/7/9 = the rocket parts found, bit 10 = the briefcase in hand.
    pub kit: u16,
    /// `DS:0x330F` (+2) — sticks of dynamite in the trapulator. The kit bit only says "some": the
    /// 2a4e take path decrements this and only clears the bit at zero, and 315f increments it (to
    /// 15) before stowing.
    pub dyn_sticks: u16,
    /// `DS:0x2F8A` (+2) — the briefcase has been picked up at least once this round.
    pub case_owned: bool,
    /// `DS:0x2F38` (+2) — the trapulator panel is open. The original keeps a *signed* blink phase
    /// there (`neg` at `CS:2d1c`), so any non-zero value means open.
    pub trap_open: bool,
    /// The sign of that same word: `FUN_26c0_2cda` negates it once per drawn frame and blits one of
    /// two variants of the "selected slot" mark, which is what makes the selection flash.
    pub trap_sign: i16,
    /// `DS:0x2F48` (+2) — **the map is up** (the trapulator's slot 12). This is a word of its own,
    /// not the panel: pressing A on slot 12 writes 1 here and jumps to the panel's own close
    /// (`CS:2A87` → `CS:2B6E`), so the panel is down for the whole look. `FUN_26c0_2b7f` draws the
    /// window into this spy's console while it is non-zero, and any fire press takes it down
    /// (`CS:2D22`).
    pub map_open: bool,
    /// The sign of that same word: the blink phase. `CS:2CD5` negates it once per drawn frame, and
    /// the **own-position** marker is drawn only on the non-negative half (`CS:2CA3`), which is the
    /// 39-px flicker stage 18b measured between consecutive map frames.
    pub map_sign: i16,
    /// `DS:0x2DF2` (+10) — the five-entry **trail**: the last five map cells this spy has stood in,
    /// newest first. `CS:26F7` fills all five with the round-start cell and `CS:36D1` shifts a new
    /// one in whenever `row_base[band] + 0x32FF` changes. The map draws the four legs between them.
    pub trail: [u16; 5],
    /// `DS:0x2F4C` (+2) — the **kit box's** blink phase, negated once per drawn frame by
    /// `FUN_26c0_31c8`. Its sign gates the icons of parts that are in the briefcase.
    pub kit_blink: i16,
    /// `DS:0x2F50` (+2) — **the Big Storm's phase**, `+1` or `-1`.
    ///
    /// The sign picks which of the two 16-px-apart copies of the flake tile at `DS:0x5FEC` is
    /// OR'd into this spy's console, and it is negated once on every frame the storm is *drawn*
    /// (`CS:0B9E`). Nothing else in the game reads, writes or tests it: the image ships `+1` and
    /// the round-start routine (`CS:04E3`) only writes the *trapulator blink* pair at
    /// `0x2F4C`/`0x2F4E`, so a later round inherits whatever sign the previous one ended on.
    /// See `STAGE23B_SPECS.md` section 1.
    pub storm_phase: i16,
    /// `DS:0x2F9E` (+2) — the trapulator cursor's repeat delay: `FUN_26c0_2a4e` moves the cursor
    /// when this is zero, then reloads 2, so a held left/right steps every third frame.
    pub cursor_delay: u16,
    /// `DS:0x2F3C` (+2) — the trapulator cursor in `slot*2` units: 0..12, 12 = the map.
    pub trap_cursor: u16,
    /// `DS:0x2F28` (+2) — "the thing in my hand is the thing to act with": set by a fire press
    /// while carrying a tool, cleared by `FUN_26c0_315f` when the tool is put away.
    pub act_latch: bool,
    /// `DS:0x2F34` (+2) — the first of the two fire taps that opens the trapulator.
    pub fire_taps: u16,
    /// `DS:0x32CB` (+2) — the action state (0 idle, 2 throwing, 0x0C..0x20 the scripted
    /// animations, 0x12 the explosion). Recorded here; the animations are a later stage.
    pub act_state: u16,
    /// **the running animation** — `DS:0x2F18` + `0x32CF` + `0x2EFC` + `0x331F` as one unit.
    ///
    /// `FUN_26c0_277c` sets `0x2F18 = 1`, the state into `0x32CF`, the pose set from `0x2CC4`, the
    /// frame word to -2 and its step to 2; the frame body then skips *all* keys while `0x2F18` is
    /// set (`CS:0727`) and advances the frame at `CS:098c` until `frame + 2 > word[0x2CE6+state]`.
    /// `None` is the idle spy. The saw and the bucket also keep the cell they will write in
    /// `cell` (`DS:0x331F`), computed **when the action starts** — that is why a recorded hole can
    /// sit two entries away from where the spy is drawn when it appears: he walked on while the
    /// twelve animation frames ran.
    pub anim: Option<Anim>,
    /// **the snowball** — `DS:0x2F40` + `0x32EF` + `0x32F3` + `0x32FB` as one value (stage 16b).
    /// `Some(..)` from the throw until a hit or the window's edge; `None` is "no ball".
    pub ball: Option<Ball>,
    /// `DS:0x2F6C` (+2) — the icicle latch: set by the ice pick (`CS:2f5b`) so that the icicle the
    /// spy is standing on cannot knock *him* out, and cleared on the first frame his feet cell is
    /// empty (`CS:244e`). While it is set the whole hazard routine returns (`CS:2457`), so it is a
    /// general hazard immunity, not just an icicle one.
    pub icicle_latch: bool,
    /// `DS:0x2F44` (+2) — the icicle-hit flag: set on the **host** of the room where the visitor was
    /// hit (`CS:250b`), and what makes `FUN_26c0_207b` blit the icicle over the up-door column. It
    /// is cleared when the victim's knockdown animation ends (`CS:283d`).
    pub icicle_hit: bool,
    /// The direction keys as the previous frame saw them, for the press edge. **port-only**, the
    /// original reads the joystick/keyboard flags directly.
    pub dir_prev: [bool; 4],
    /// A trapulator left/right request that has been seen but **not consumed yet**. `-1` left,
    /// `+1` right, `0` none.
    ///
    /// The original's `DS:0x328F`/`0x3293` are *latched press flags*, not key levels: the keyboard
    /// handler sets one to 1 and only `FUN_26c0_2a4e` clears it, at `CS:2b0a` (left) or `CS:2b49`
    /// (right), and only on the frame it actually moves the cursor. While `DS:0x2F9E` is counting
    /// down the routine returns at `CS:2afd`/`2b3c` **without** clearing the flag, so a press that
    /// arrives during the repeat delay is still served two frames later. **port-only** field (the
    /// original keeps it in that flag word); it is what stops a press shorter than one 100 ms game
    /// frame from being lost.
    pub cursor_req: i8,
    /// `DS:0x2FA4` (+2) — **my last movement step was refused because the other spy's body is in
    /// the way** (`CS:3667`, set by `FUN_26c0_35e5` and cleared at its top, `CS:35e5`). The
    /// original reads it back only in the attract-mode autopilot (`CS:1774`), so it changes nothing
    /// a player sees; the port keeps it because it is the only observable trace of the contact and
    /// `--state` reporting it is what the tests assert.
    pub blocked: bool,
    /// `DS:0x2F58` (+2) — **body heat, counted as cold**: 3 is a full bar, `0x3D` is the last
    /// survivable value and `0x3F` is as far as the table lets it go (stage 14).
    pub heat: u16,
    /// `DS:0x2F60` (+2) — the per-terrain rate counter, shared by every branch of
    /// `FUN_26c0_1e0f` (deep snow steps it every 8 frames, water every 12, the igloo floor every
    /// 21, the fire every 2).
    pub terrain_t: u16,
    /// `DS:0x2F54` (+2) — the base-drain counter: one `+2` cold step every `0x8A` frames, cleared
    /// by *any* heat change (`FUN_26c0_21bd`'s tail writes 0 here).
    pub base_t: u16,
    /// `DS:0x31AA` — **this pass's move was refused.**
    ///
    /// Set by exactly one routine, `FUN_26c0_3396` (`CS:3398` is `movw $1,0x31AA`), and cleared by
    /// exactly two, both in the shove's arms (`CS:0AAB`/`0x0AE4`). Nothing else in the game reads
    /// it: the only read sites are `CS:0AB9`/`0AF4` — "the dispatch stopped him, so undo the shove".
    /// The port therefore sets it in [`SpyState::stop`] and reads it only inside
    /// [`Game::push_in_step`], which clears it before its own dispatch.
    pub refused: bool,
    /// `DS:0x2F64` (+2) — `0` alive, `1` frozen (stopped, the death animation has run), `2` out of
    /// the round. The launch writes 2 as well; [`SpyState::out_of_round`] is that same word from
    /// the other side, so the two are kept in step.
    pub frozen: u16,
}

/// One running animation (`FUN_26c0_277c` + the frame body's advance at `CS:098c`).
///
/// The three words the original keeps per spy, as one value: the state (`DS:0x32CF`), the frame
/// (`DS:0x2EFC`) and — for the saw and the bucket — the cell the animation will write when it ends
/// (`DS:0x331F`, stored at the start of the action).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anim {
    pub state: u16,
    /// `DS:0x2EFC`, signed: seeded with -2 and stepped by 2; the value drawn is the one *after*
    /// the step.
    pub frame: i16,
    pub cell: Option<usize>,
}

/// **A snowball** — the four words `FUN_26c0_2f87` writes and `CS:303c` reads, as one value.
///
/// The ball is *not* an object: nothing is added to the object blocks (measured: both spies' blocks
/// are byte-identical across the recorded throws), it is drawn as its own sprite record, and it
/// exists only between the throw's frame 8 and the edge of the window or the other spy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ball {
    /// `DS:0x32EF` — the sprite's **bottom** row, `y - 0x12` at the throw and never changed.
    pub row: i32,
    /// `DS:0x32F3` — the sprite's left column, in world bytes. Stepped by `vx` a frame.
    pub col: i32,
    /// `DS:0x32FB` — +3 eastward, -3 westward.
    pub vx: i32,
    /// `DS:0x2F40` — in flight. `CS:09b8` sets it on the throw's frame 8; a hit (`CS:30e2`) or the
    /// window's edge (`CS:3155`) clears it. Before frame 8 the three words above are written but the
    /// ball is neither moved nor drawn.
    pub flying: bool,
}


/// The carried word as the report prints it: -1 for an empty hand.
pub fn describe_carried(c: u16) -> i32 {
    if c == CARRY_NONE { -1 } else { c as i32 }
}

/// The mechanical name of a carried code (see [`items::item_name`]).
pub fn carried_name(c: u16) -> &'static str {
    items::item_name(c)
}

/// `DS:0x2EB8` at every round start: bit 10 is a constant base flag, not an item (measured in
/// every dump of every session; the item bits 6/7/9 are added by a deposit).
pub const MASK_BASE: u16 = 0x400;
/// The carried word's "nothing" value, re-exported for the binary's reporting.
pub const CARRY_NONE_UI: u16 = CARRY_NONE;


/// The walk-cycle frames the original's counter bounces through (see [`WALK_FRAMES`]).
pub const WALK_SEQUENCE: [u16; 4] = WALK_FRAMES;

// ---- stage 14: body heat, sinking and the freeze ------------------------------------------
/// `DS:0x2F58`'s full bar — the round starts here (measured in every dump of every session).
pub const HEAT_FULL: u16 = 3;
/// `CS:0745`: the freeze test is `heat > 0x3D`, so 61 is the last survivable value.
pub const HEAT_FREEZE_AT: u16 = 0x3D;
/// `CS:21f4`: the add is refused once the word is above 0x3D, so 0x3F is the highest it reaches.
pub const HEAT_MAX: u16 = 0x3F;
/// `DS:0x2F54` — the base-drain counter's period (`CS:0753`).
pub const HEAT_BASE_PERIOD: u16 = 0x8A;
/// The water branch's sink plateau (`CS:1ee5`: 0x16 is written and clamped back to 0x14).
pub const SINK_WATER: u16 = 0x14;
/// The deep-snow branch's plateau (`CS:1eac`: 8 is written and clamped back to 6).
pub const SINK_SNOW: u16 = 6;
/// `FUN_26c0_2238` sets the frozen pose set to this **before** it starts the animation, which is
/// why the animation's "restore" puts `0x72` back and the statue keeps it for good.
pub const FREEZE_POSE_SET: u16 = 0x72;

// ---- stage 20a: the DAMAGE model — what the traps cost in body heat -------------------------
//
// `FUN_26c0_21bd` takes **AX = a signed count of cold steps**, each one `+2` on the heat word
// (`CS:21d6`..`21fb`), and then composes and blits that spy's bar. It contains **no sound call**:
// stages 13a, 13b and 16b read the negative AX at its call sites as a "sound id" and left the
// damage out. Every number below is that `mov $imm,%ax` read out of the image, and every one of
// them is also measured from the recorded corpus (`stage20a_probe.py events`), which is the point
// of the table: the arithmetic and the recordings agree source by source.
//
// Evidence level: **read** (the call site and its immediate) and **measured** (the heat word
// across the recorded dump pair that holds the event — 3 blasts, 4 holes, 10 ice slips, 2
// icicles, 44 snowball hits, 3 spills, all in `recon/round/sessions/`).

/// `CS:24b4` — the **saw hole**: `mov $0xFFFD,%ax` = 3 steps = **6 cold**, measured 6 in 3 of 4
/// recorded falls (the fourth is 8: its base counter `0x2F54` stood at 0x89, so one base step
/// landed on the same frame — `rocket-room-3-hard` ds_01094).
pub const DAMAGE_HOLE_STEPS: i32 = 3;
/// `CS:1e48` — the **ice terrain's** slip: the same `mov $0xFFFD,%ax`, no object byte involved.
/// Measured 6 in 10 of 10 recorded slips. The spill the *bucket* leaves (`CS:2473`) has no
/// `21bd` call at all — see [`DAMAGE_SPILL_STEPS`].
pub const DAMAGE_ICE_STEPS: i32 = 3;
/// `CS:2e21` (the plunger) and `CS:37de` (a tap on the other spy's stick): `mov $0xFFFB,%ax` =
/// 5 steps = **10 cold**. Measured 10 in 3 of 3 recorded detonations.
pub const DAMAGE_BLAST_STEPS: i32 = 5;
/// `CS:2511` — the **icicle**: the same `mov $0xFFFB,%ax`, 10 cold. Measured 10 in 2 of 2.
pub const DAMAGE_ICICLE_STEPS: i32 = 5;
/// `CS:30ee` — the **snowball**: `mov $0xFFFE,%ax` = 2 steps = **4 cold**, on the victim (`SI` is
/// switched to him at `CS:30e9` before the call). Measured 4 in 43 of 44 recorded hits; the 44th
/// reads 2 because the word was at 61 and `CS:21f4` refuses to pass 0x3D (`playtest5` ds_01633).
pub const DAMAGE_SNOWBALL_STEPS: i32 = 2;
/// **Zero**, and it is a real zero: the spill branch (`CS:2473`..`24a6`) clears the byte, starts
/// the laugh on the other spy and the slip on the victim, and never calls `21bd`. Measured 0 in 3
/// of 3 recorded spill bites. The owner's "slipping on an ice patch does not remove heat" is this
/// branch — a *spilt* patch of ice — while the ice *terrain*'s slip costs 6.
pub const DAMAGE_SPILL_STEPS: i32 = 0;

/// The heat bar's left column (`DS:0x2F5C` = 8 in the original's own blit units; measured at
/// x 58 on the plate, with the gauge's casing one column either side).
pub const HEAT_BAR_X: usize = 58;
/// The bar's width, casing excluded: the nine columns x 58..66 (the ROI's `w = 9`).
pub const HEAT_BAR_W: usize = 9;
/// The bar's height in rows — one row per cold unit over the 3..62 range (`h = 59` in the ROI).
pub const HEAT_BAR_ROWS: usize = 59;
/// The top console's first bar row, and the bottom console's (`9 + 108`).
pub const HEAT_BAR_TOP_Y: usize = 9;
pub const HEAT_BAR_BOTTOM_Y: usize = 117;
/// Where the plate already carries a **drained** (white) row — rows 9..16 are white — and a
/// **remaining** (cyan) one, rows 17..67. The two rows of each pair are the CGA dither's parity.
pub const HEAT_BAR_WHITE_Y: usize = 9;
pub const HEAT_BAR_CYAN_Y: usize = 17;

impl SpyState {
    pub fn x(&self) -> i32 {
        self.x3 / SUB
    }
    pub fn y(&self) -> i32 {
        self.y3 / SUB
    }
    /// `FUN_26c0_3396` — nothing is held, so the spy comes to rest: both velocities zero, the
    /// frame step zero, and the legs put back on the standing frame. This is the routine's whole
    /// body (`CS:3396`: `0x2F04 = 0; 0x2F00 = 0; 0x2F08 = 0; 0x2EFC = 2` plus the two saved
    /// positions), and it is why a spy never stops mid-stride in the original.
    pub fn stop(&mut self) {
        self.vel_x = 0;
        self.vel_y = 0;
        self.frame_step = 0;
        self.frame = STAND_FRAME;
        // `CS:3398` — the same three instructions end with `movw $1,0x31AA`: "this pass was
        // refused". Only the shove reads it (`CS:0AB9`), to decide whether to undo its own move.
        self.refused = true;
    }
    /// One game frame's step of the animation counter (`CS:0a06`):
    /// `if (frame + step < 0 || frame + step > 0x2F10) step = -step; frame += step;`
    pub fn advance_frame(&mut self) {
        if self.frame_step == 0 {
            return;
        }
        let next = self.frame as i32 + self.frame_step as i32;
        if next < 0 || next > ANIM_MAX as i32 {
            self.frame_step = -self.frame_step;
        }
        self.frame = (self.frame as i32 + self.frame_step as i32).clamp(0, ANIM_MAX as i32) as u16;
    }
    /// Foot row as drawn: the sprite's rows are `y-28 ..= y-1`, so this is `DS:0x3297`.
    pub fn col_offset(&self) -> i32 {
        self.x() - CELL * (self.x() / CELL)
    }
    pub fn col(&self) -> usize {
        (self.x() / CELL).clamp(0, 11) as usize
    }
}

/// A small deterministic RNG so a seed reproduces a round exactly (the original uses `FUN_26c0_20cd`
/// for the same jobs; this is not that generator, and the port does not claim to match its stream).
#[derive(Clone, Debug)]
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Rng {
        Rng(seed.wrapping_mul(2654435761).wrapping_add(1))
    }
    /// The raw state, for a test that must show a stream was **not** consumed.
    pub fn state(&self) -> u32 {
        self.0
    }
    pub fn next(&mut self) -> u32 {
        // xorshift32
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}

/// How the igloo door opens — see [`RoundOpts::igloo_entry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IglooEntry {
    /// Walk onto the mask's `T_IGLOO_IN` cell (the original's `AX == 7` branch).
    Walk,
    /// Stage 4's reading: the action key while standing in the alcove.
    Key,
    /// The cell is walkable and does nothing (what stage 4 shipped).
    Off,
}

/// Options for starting a round. Every unknown is a knob rather than a hard-coded guess.
#[derive(Clone, Debug)]
pub struct RoundOpts {
    pub island: usize,
    pub players: usize,
    pub seed: u32,
    /// Force the round-start placement's PRNG state instead of deriving it from `seed`.
    ///
    /// The original's placement PRNG (`FUN_26c0_20cd`) is an 8x8 multiply whose `DX` comes from the
    /// DOS clock at every draw, except on the attract-mode branch, where `DS:0x3203` is non-zero,
    /// `DX` is the fixed `0x770D` and `DS:0x31B0` is seeded with `0x469`. `Some(0x469)` therefore
    /// reproduces the attract round's layout exactly, which is what the acceptance test does.
    /// `None` (the default) derives a state from `seed`.
    pub obj_state: Option<u16>,
    /// **The original's seeding for the placement: `DX` from the DOS clock, not the attract round's
    /// fixed `0x770D`.**
    ///
    /// `FUN_26c0_20cd` issues `INT 21h AH=2Ch` on *every* call unless the attract-mode flag
    /// `DS:0x3203` is set (see [`Self::obj_state`]), so a normal round's item layout is a fresh
    /// function of the wall clock — and `CS:060D` re-enters `0x440` on every round, which is why
    /// every GO! and every restart in the original has a different layout. `--obj-clock`; see
    /// [`round::ObjRng::from_clock`] and [`round::CLOCK_TICK_DRAWS`] for how the reading advances.
    ///
    /// **Off by default**: the port's tests, probes, soaks and demos are pure functions of their
    /// options, and a clock-seeded round is not. `--play` turns it on (unless `--seed`/`--obj-state`
    /// pins the layout); `--seed N` turns it back off for a reproducible session.
    ///
    /// Unlike the original, the port's `DS:0x31B0` starts fresh each round instead of carrying over
    /// from the previous round's draws — a fresh [`round::ObjRng::from_clock`] per `Game::new`.
    pub obj_clock: bool,
    /// A **stand-in for the clock reading** in [`Self::obj_clock`] mode: the `(dh, dl)` an injected
    /// `INT 21h` reports at the round start, so two runs can be made to differ deterministically.
    /// `None` = the real clock. Tests and probes only; there is no command-line flag for it.
    pub obj_clock_dx: Option<(u8, u8)>,
    /// Reproduce the evidenced random placement instead of the deterministic default.
    pub random_start: bool,
    /// Force a start screen / x / y (mainly for tests and for the recorded walk).
    pub start_screen: Option<u16>,
    pub start_x: Option<i32>,
    /// Player 2's own start x, when a round wants the two spies apart on one screen.
    pub start_x2: Option<i32>,
    pub start_y: Option<i32>,
    /// Override the island's round clock, in seconds. `None` = the island's own (measured).
    pub clock_override: Option<u16>,
    /// Treat the empty/mountain terrain code (`T_CLIMB`, level 8 = the "ocean"/void band the owner
    /// saw spies standing in) as solid. **Off by default**: the original's mask gives those cells
    /// code 1 and `FUN_26c0_1e0f` lets a spy walk on them (see `STAGE4_REPORT.md` §1.3). The flag
    /// exists because the owner expects the water to block, and a wrong guess should cost a
    /// keypress rather than a rebuild.
    pub void_solid: bool,
    /// How the igloo door opens. `Walk` (the default) is `FUN_26c0_1e0f`'s own `AX == 7` branch:
    /// the spy walking onto the mask's door cell is the trigger. `Key` is stage 4's reading (the
    /// action key at the alcove, `IGLOO_ENTER_CX`/`_Y`), kept as a one-keypress fallback. `Off`
    /// leaves the cell walkable and inert, which is what stage 4 shipped.
    pub igloo_entry: IglooEntry,
    /// **stage 23c — port-only verification switch** (`--no-storm`): suppress the Big Storm's
    /// overlay so a console can be rendered without it. The original has no such switch (its gate
    /// is the clock, `CS:0B57`); this exists only for the before/after contact sheet.
    pub no_storm: bool,
    /// Draw the visitor into the host's console (`FUN_26c0_0c93`'s two-spy loop). **On by
    /// default**; off gives the stage-4 composition (only the console's own spy), which is what the
    /// report's before/after counts compare against.
    pub visitor_draw: bool,
    /// Re-test the shared-screen visitor flag only in the **moving** spy's frame (stage 4's
    /// behaviour). Off by default: `FUN_26c0_0000` runs for both spies on every frame of the
    /// per-player loop, moving or not, so the flag is re-tested every frame and a host walking away
    /// clears the visitor's console (see `STAGE5_REPORT.md` defect C).
    pub visitor_eval_mover_only: bool,
    /// The two sprite layers. **Both default on**: since stage 2A each is reproducible from the
    /// executable's own tables (objects: raw two-plane records; spy: self-masking records), and
    /// together with the plates they reproduce 881 of 1082 recorded strips byte for byte. `--objects`
    /// / `--spies` still exist so a layer can be turned *on* explicitly, and the struct keeps them
    /// switchable for the probes that compare layers.
    pub objects: bool,
    pub spies: bool,
    /// Draw the trapulator dot strip over the lifted surround. On by default; the strip is where a
    /// carried tool shows up, so a run without it cannot show the trapulator working.
    pub items_ui: bool,
    /// The setup screen's launch-site option: `false` = HIDE, `true` = SHOW (`DS:0x31D7` 0 or 2,
    /// copied into the round's `DS:0x31D5` at setup, `CS:120a`). HIDE keeps the rocket hatch **out
    /// of the object block** until the briefcase holds all three parts (`CS:2696`, `CS:0610`);
    /// SHOW inks it at placement, which is the "launch site on SHOW" the setup screen offers.
    pub launch: bool,
    /// Put spy 1 on the hatch's own cell after the placement (a demo/test aid, like `start_x`:
    /// it consumes no PRNG draws and does not move the hatch). `--start-hatch`.
    pub start_hatch: bool,
    /// **Who the computer drives** — `--cpu1` / `--cpu2`, and the setup screen's one-player
    /// option (which drives spy 2, as the original does at `CS:039f`). `false` = the keyboard.
    pub cpu: [bool; 2],
    /// The options screen's **difficulty**, 0..4 (displayed 1..5). The original keeps it in
    /// `DS:0x31BF`, stepped by 2 (`CS:1066`), and the round reads it straight from there — no copy
    /// is made at round start. `--difficulty N` (1..5) on the command line.
    pub difficulty: u16,
    /// **demo aid** (`--carry-at FRAME:CODE`, `--carry2-at FRAME:CODE`): put an item code into a
    /// spy's hand at a given **game frame**, so a panel can plant a stick with one hand and detonate
    /// it with the other. Same semantics as [`Self::carry`]; nothing in the engine reads it.
    ///
    /// The unit is the *frame*, like every other duration in this engine (stage 16a): it was a tick,
    /// which meant a demo aid's meaning moved when the frame period did — `--carry-at 20:1` was
    /// frame 3 at 6 ticks a frame and frame 0 at 26.
    pub carry_at: Vec<(usize, u32, u16)>,
    /// **demo aid** (`--heat N`): seed both spies' body-heat word, so a panel can show the bar at a
    /// chosen fill, or the freeze, without waiting 138 frames per step (`DS:0x2F58` is otherwise
    /// only ever written by `FUN_26c0_21bd`).
    pub heat: Option<u16>,
    /// **demo aid** (`--heat2 N`): seed only the *second* spy's heat, so a panel can freeze one spy
    /// while the other keeps playing (the `--heat` aid always seeds both).
    pub heat2: Option<u16>,
    /// **demo aid** (`--parts`): set `kit |= 0x2C0` on both spies — "all three parts are in the
    /// briefcase" (`CS:284d`'s test). No recorded round needs it; the recordings find the parts.
    pub parts: bool,
    /// **demo aid** (`--carry`, `--carry2`): put an item code straight into a spy's hand after the
    /// placement, as a trapulator take would. `None` = leave the hands empty. It runs through
    /// `set_carried`, so the kit's case bit and the carrying pose follow, and it is only used by
    /// the stage scripts: no recorded round needs it.
    pub carry: Option<u16>,
    pub carry2: Option<u16>,
    /// **The frame's length in ticks**, for sweeping the period (`--fps N`). `None` = the
    /// original's 26 ticks of the 240 Hz timer (`FRAME_TICKS`, 108.34 ms). Nothing else about the
    /// engine changes with it: the wait, the clock's tick word, heat's 138 and the ascent's 119 are
    /// all counted in frames, so a sweep scales every wall-clock rate together, which is what makes
    /// it a *period* sweep and not a different game.
    pub frame_ticks: Option<u32>,
    /// **The two spies' bodies.** On by default: `CS:097c` calls `FUN_26c0_35e5` on every step, and
    /// that routine refuses a step that would bring the mover within 3 units of the other spy on the
    /// same row. `--spy-block off` restores the stage-8 behaviour (spies walk through each other) so
    /// the defect and its fix can be measured side by side on the same recording.
    pub spy_block: bool,
    /// **Stage 24a — the attract demo.** `Some(masks)` runs the round as the panel's idle timeout
    /// does: one recorded key mask a frame, both spies, no keyboard and no computer
    /// (`CS:0325` -> `CS:0203`). `None` = an ordinary round.
    ///
    /// The masks are `DEMO.DAT` parsed by [`round::demo_masks`]; frame *n* uses `masks[n]`, and past
    /// the end every frame gets mask 0 (the original reads whatever the load left beyond the file —
    /// zero in both recorded demos — and never rewinds `DS:0x0FB6`).
    ///
    /// A demo round is created with `obj_state: Some(0x469)` (the attract PRNG state), `island: 0`
    /// and `launch: true`; [`Game::new`] then forces `players = 2` and clears `cpu`, because the
    /// recorded stream drives both spies whatever the options screen said (`DS:0x31DD` is only ever
    /// read by the input scan, which the attract branch bypasses).
    pub demo: Option<Vec<u16>>,
}

impl Default for RoundOpts {
    fn default() -> Self {
        RoundOpts {
            island: 0,
            players: 1,
            seed: 0,
            obj_state: None,
            obj_clock: false,
            obj_clock_dx: None,
            random_start: false,
            start_screen: None,
            start_x: None,
            start_x2: None,
            start_y: None,
            clock_override: None,
            void_solid: false,
            igloo_entry: IglooEntry::Walk,
            no_storm: false,
            visitor_draw: true,
            visitor_eval_mover_only: false,
            objects: true,
            spies: true,
            items_ui: true,
            launch: false,
            start_hatch: false,
            cpu: [false, false],
            difficulty: 0,
            carry: None,
            carry2: None,
            carry_at: Vec::new(),
            heat: None,
            heat2: None,
            parts: false,
            frame_ticks: None,
            spy_block: true,
            demo: None,
        }
    }
}

/// The rocket hatch and the polar bear it can let out — `FUN_26c0_2392`, the state machine of
/// `FUN_26c0_277c` / `FUN_26c0_2807` and the four `DS` words that carry it.
///
/// ```text
/// walk onto a hatch-family cell    [0x31B6] = 0x10, animation 0x1E starts   (CS:2417)
/// state 0x1E ends                   kit & 0x2C0 == 0x2C0 ?  the rocket   (CS:284d)
///                                   : animation 0x20
/// ten frames into 0x20              [0x31B6 + 2] = 0x11  -- THE POLAR BEAR (CS:23b7)
/// state 0x20 ends                   animation 0x1C (and 0x0C for the other spy)
/// state 0x1C ends                   [0x31B6] = 0x0F, [0x31B6+2] = the saved byte,
///                                   0x32CF = 0, 0x2F74 = 0                    (CS:292d)
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HatchSeq {
    /// `DS:0x32CF` = 0 — nothing running.
    Idle,
    /// `0x1E` — the spy has stepped onto the hatch and is climbing down into it.
    Descend,
    /// `0x20` — the descent without all three parts: ten frames in, the bear comes out.
    Bear,
    /// `0x1C` — climbing back out; the hatch closes and the bear's byte is restored at the end.
    Close,
    /// The rocket branch: `kit & 0x2C0 == 0x2C0`, so the hatch launches instead of the bear.
    /// The ascent itself is a later stage (see the report's open items).
    Launched,
}

impl HatchSeq {
    /// The animation state word this stage maps to (`DS:0x32CF`).
    pub fn state(self) -> u16 {
        match self {
            HatchSeq::Idle | HatchSeq::Launched => 0,
            HatchSeq::Descend => crate::round::STATE_DESCEND,
            HatchSeq::Bear => crate::round::STATE_BEAR,
            HatchSeq::Close => crate::round::STATE_CLOSE,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            HatchSeq::Idle => "idle",
            HatchSeq::Descend => "descend",
            HatchSeq::Bear => "bear",
            HatchSeq::Close => "close",
            HatchSeq::Launched => "launched",
        }
    }
}

/// The four words the hatch sequence owns. Kept next to the game state because they are what
/// `--state` reports and what the demos assert on.
#[derive(Clone, Copy, Debug)]
pub struct HatchState {
    /// `DS:0x31D5` — 0 = HIDE (and not revealed yet), 2 = in the block (SHOW, or revealed).
    pub armed: u16,
    /// `DS:0x31B6` — the address of the hatch's middle entry in the object block.
    pub addr: Option<usize>,
    pub seq: HatchSeq,
    /// Which spy is in the animation (`DS:0x32CF` + 2*spy).
    pub spy: usize,
    /// Frames elapsed in the current state (`DS:0x2EFC` = -2 + 2*frames).
    pub frames: u16,
    /// `DS:0x2F74` — frames spent in state `0x20`; ten spawns the bear.
    pub bear_frames: u16,
    /// `DS:0x31B8` — the byte the bear's spawn overwrites, restored when the hatch closes.
    pub saved: u8,
    /// Whether the bear's byte is in the block right now.
    pub bear: bool,
    /// The *other* spy, while state `0x0C` runs on him (`CS:2915`).
    pub other: Option<usize>,
    /// The script's **x velocity** for the frame last applied (`CS:9ea`: the second column is
    /// written into the memory that `DS:0x3293` points at, i.e. into the **right-key flag word**).
    /// That is how the throw scrolls the view: the flags are physical, but the script overwrites
    /// the right one, so the view follows the arc's eastward drift and nothing else.
    pub vx: i32,
    /// The keys each spy's frame was given, so the scroll can be run with them after the script has
    /// been applied (the original's order: script, commit, scroll — `CS:09ca..0a3e`).
    pub keys: [Keys; 2],
}

impl Default for HatchState {
    fn default() -> Self {
        HatchState {
            armed: 0,
            addr: None,
            seq: HatchSeq::Idle,
            spy: 0,
            frames: 0,
            bear_frames: 0,
            vx: 0,
            keys: [Keys::default(); 2],
            saved: 0,
            bear: false,
            other: None,
        }
    }
}

// ---- the game ------------------------------------------------------------
pub struct Game {
    pub data: IslandData,
    pub levels: [[u16; 12]; 10],
    pub spies: [SpyState; 2],
    pub players: usize,
    /// The round-start object layout (`round::place_round_objects`), for reporting and for the
    /// pickup rule. The blocks themselves live in `ds`; this is the same information as a list.
    pub layout: round::ObjectLayout,
    /// `DS:0x3219/0x3205/0x320F/0x3223` — the five-entry **position record** the computer reads
    /// (entry 0 the hatch, 1..3 the parts, 4 the case). Recorded at the round start (`CS:25c6`,
    /// `CS:265d`), invalidated (`screen = 0xFFFF`) when a part or the case is taken
    /// (`CS:383f`/`CS:387f`) and re-recorded when one is put down (`CS:39b0`, `CS:2327`).
    pub links: [cpu::Link; 5],
    /// `DS:0x324D` — the round clock, in **counts** (the seconds the pod shows). `CS:22a7`
    /// decrements it; this field *is* that word, and [`Game::clock_secs`] is what the pod draws.
    ///
    /// It is a count of a **count**, not of ticks: `CS:0680` tests the tick word below once a
    /// frame, so one count is 9 frames = 975.1 ms (stage 16a). A 360-count island round therefore
    /// lasts **351 s** of wall clock, not 360.
    pub clock_left: u16,
    /// `DS:0x33F6` — the clock's own tick word: +1 a tick (240 Hz), compared with 0xDC (220) **once
    /// per frame** and cleared when it fires (`CS:0680`..`CS:068e`). With a 26-tick frame that is 9
    /// frames a count; the field is kept word-for-word rather than folded into a frame counter so
    /// the arithmetic stays the original's.
    pub clock_word: u16,
    pub clock_secs_total: u16,
    pub ticks: u64,
    /// Game frames run since the round started. One frame is [`Game::frame_ticks`] ticks.
    pub frames: u64,
    /// Ticks a game frame waits for: [`FRAME_TICKS`] (26) unless a sweep (`--fps`) changed it.
    /// Nothing else reads it — the wait, the clock's word and the trace all measure in it.
    pub frame_ticks: u32,
    pub phase: Phase,
    pub objects: bool,
    pub spies_layer: bool,
    pub void_solid: bool,
    pub igloo_entry: IglooEntry,
    /// Draw the visitor's sprite into the host's console (see [`RoundOpts::visitor_draw`]).
    pub visitor_draw: bool,
    /// **The storm's draw record** — whether each console was snowed on the previous frame.
    ///
    /// `CS:0B9E` negates the word *after* the draw, so the value frame `N` renders with is the
    /// negation of the value frame `N-1` rendered with. The port renders after the frame, so the
    /// flip must happen at the **top** of the next frame and must know what the last draw did:
    /// this is that record, written at the end of each frame body (nothing runs between the end
    /// of the frame and the draw, so the eligibility is the same value).
    pub storm_drawn: [bool; 2],
    /// **port-only verification switch** (`--no-storm`): suppress the Big Storm's overlay. Nothing
    /// in the original switches it off — the gate is the clock — so this exists only so a console
    /// can be rendered "stormless" for a before/after comparison (`stage23c_storm.png`).
    pub storm_off: bool,
    /// Stage 4's visitor rule (test only in the moving spy's frame). Off by default.
    pub visitor_eval_mover_only: bool,
    /// `CS:097c` / `FUN_26c0_35e5` — the two spies' bodies block each other (see
    /// [`RoundOpts::spy_block`]).
    pub spy_block: bool,
    pub exe: Vec<u8>,
    /// The per-level behaviour mask the original tests every move against (`FUN_26c0_366e`).
    pub terrain: Terrain,
    /// Ticks into the current game frame (0..`FRAME_TICKS`). The original's loop is 10 Hz; this
    /// engine ticks at 60 Hz and runs one game frame every six ticks.
    pub frame_phase: u32,
    /// The keys seen down at **any** tick since the last game frame, per player — the frame latch
    /// described in [`Game::tick`]. Cleared when the frame consumes it.
    pub pending: [Keys; 2],
    /// The 64 KB data segment the viewport is composed from: the static image, with this round's
    /// level table and the spies' own variables patched in before each compose.
    ds: Vec<u8>,
    /// The port's own PRNG for the one roll the terrain makes (`FUN_26c0_20cd`'s stand-in, the ice
    /// slip at `CS:1e3a`). Seeded from `RoundOpts::seed`, so a seed reproduces a slip.
    pub rng: Rng,
    /// **stage 24b** — `DS:0x31B0` and the draw count **after the round-start placement**: the
    /// state the shared generator is left in for the rest of the round. The attract round's first
    /// recorded dump holds `0x090A` and every one of the 244 demo dumps holds it unchanged, so the
    /// placement's draw order is pinned by that word and a round may not draw again after it.
    pub placed_rng_state: u16,
    pub placed_rng_draws: u32,
    /// **The deep-snow trudge's own stream** (`CS:1e95`'s `call 0x20cd`, stage 23c).
    ///
    /// It is deliberately **not** `self.rng`: that one is the AI's and the ice's, and taking an
    /// extra byte per deep-snow pass from it would re-time every seeded AI sequence and every
    /// `every_recorded_*` replay. Seeded from `RoundOpts::seed` like the rest, so headless runs
    /// stay byte-identical and a seed still reproduces a walk.
    pub stand_rng: Rng,
    /// **test/verification hook** — a fixed trudge roll (`None` = consume `stand_rng`). Never set
    /// by the round itself; it lets a test force an even or an odd byte on every pass.
    pub stand_roll: Option<u8>,
    /// **stage 24f** — whose frame body has already run in the current frame, in the original's
    /// order (`CS:0702`: player 2 first). `FUN_26c0_277c` seeds an animation at `0x2EFC = -2`; the
    /// spy's own body steps that seed in the same frame if it has not run yet, and only the draw's
    /// `CS:0d44` spends it if it has. [`Game::draw_clamp`] needs the difference.
    pub body_ran: [bool; 2],
    /// **stage 24f** — `0x2EFC` is still holding `FUN_26c0_277c`'s `-2` seed for this spy *after* his
    /// own body ran: an animation armed on him by somebody else (the reaction laugh, the plunger's
    /// blast, or a ball of the other player's). The draw's `CS:0d44` spends it — see
    /// [`Game::draw_clamp`]. An animation armed inside his own body is stepped there (`CS:098c`) and
    /// never comes here.
    pub seed_pending: [bool; 2],
    /// **stage 25a** — `DS:0x32D7`, each spy's cached feet cell: the address `FUN_26c0_2549`
    /// computes from `0x3297`/`0x329B`/`0x329F` and writes at `CS:0afd`, the last thing in that
    /// spy's frame body.
    ///
    /// Every reader runs **earlier in the next frame** — the hazard under the feet (`CS:2444`), the
    /// hatch touch (`CS:2392`), the take (`CS:373b`), the icicle (`CS:24f2`) and the plunger
    /// (`CS:2e06`) — so what they see is the cell the spy stood on at the end of the previous frame,
    /// never the trial cell the move is about to test (`CS:944` probes *that* one separately, and
    /// only for the terrain). The port's `cell_of` computes from the live position, which is the
    /// same value for every reader that runs before its own frame's commit — but **not** for the
    /// hatch touch, which the port runs after the bodies: there the difference is one frame, and it
    /// is the demo's whole descent residual (`STAGE25A_REPORT.md`).
    pub feet_addr: [Option<usize>; 2],
    /// Last frame's [`Game::feet_addr`] — what a reader inside a body sees.
    pub feet_prev: [Option<usize>; 2],
    /// The console surround and the clock glyphs — lifted from captures (`round::RoundAssets`).
    pub assets: round::RoundAssets,
    /// `DS:0x2EB8` — the **deposited** ledger: a rocket part's bit is set when the part goes into
    /// the briefcase, and it never clears in a round. The round-start value is the constant
    /// `0x400` base bit. One word, not one per player: `FUN_26c0_373b` writes it with an absolute
    /// address (`OR AX,0x2EB8` at `CS:3852`) while it indexes the *player's* kit word with `SI`,
    /// so on this build both spies share it. That is why no separate P2 mask was ever found.
    pub mask: u16,
    /// `DS:0x322D` — the four part-owner words (codes 9..12): `0xFFFF` = nobody, `0` = the white
    /// spy, `2` = the black spy. This is the **per-spy** ledger the kit box draws from; the mask
    /// above is shared, this is not. Written on a pickup or a deposit of a part (`CS:3886`,
    /// `CS:383a`).
    pub part_owner: [u16; items::PART_OWNERS],
    /// Draw the trapulator's dot strip (the port's own addition to the lifted surround; the dots
    /// themselves are the measured 2x2 cells of `profile/rois.toml`).
    pub items_ui: bool,
    /// The scoreboard of what the items did, for `state_json` and the report:
    /// (picked up, dropped, buried, dug, stowed, taken, deposited, hazards placed, hazards sprung,
    /// sticks blown off the ice, plunger pulls).
    pub item_counts: [u32; 11],
    /// **demo aid** (`--carry-at`): (player, tick, item code) applied when the tick arrives.
    pub carry_at: Vec<(usize, u32, u16)>,
    /// The rocket hatch: its armed word, its address, and the position in the descent/bear
    /// sequence (`FUN_26c0_2392`).
    pub hatch: HatchState,
    /// `DS:0x2FB0` — how many of this island's igloo interiors got a fire at round start.
    pub fires: u16,
    /// `DS:0x2F70` — non-zero while the rocket is in flight (`CS:28ae`). It keeps the launched spy
    /// out of the player loop (`CS:71d` skips his whole frame) and out of the sprite layer, where
    /// `0xc93` draws the rocket instead.
    pub rocket: bool,
    /// `DS:0x2EBC` — the **ascent counter**: `+2` a frame from 2 to [`round::ASCENT_END`] (238),
    /// and at 238 the round-over flag goes up (`CS:20f2`, `CS:21ae`). 119 frames in all.
    pub rocket_asc: u16,
    /// `DS:0x2EBE`-equivalent: the rocket's world column, `2 * col` where `col` is the hatch's
    /// column (`DS:0x320f`). At the launch the spy stands on that column (`CS:2436` writes
    /// `x = 2*entry + 2`), so `launch` takes it from his own x and it never moves again.
    pub rocket_x: i32,
    /// `DS:0x248A` — the ascent's own round-over flag (`CS:21b6`). The outer loop clears it and
    /// goes back to the options screen (`CS:0649`).
    pub over_flag: bool,
    /// Which exit took the round out, once [`Phase::Over`] is set.
    pub end: Option<RoundEnd>,
    /// The tick on which the round ended (headless reports use it to ignore the frames after).
    pub end_tick: u64,
    /// The computer opponent, one per spy (`None` = that spy is played by the keyboard). In the
    /// original only spy 2 can be a computer (`CS:0348`, `DS:0x31DD == 1`); the port allows either
    /// so the routine can be exercised from both sides.
    pub cpu: [Option<cpu::Cpu>; 2],
    /// **Stage 24a** — the attract demo's recorded key masks, one a frame (`DEMO.DAT`). `None` = an
    /// ordinary round. While it is `Some`, [`Self::tick`] takes the frame's keys from it and from
    /// nothing else: `CS:0325` -> `CS:0203` replaces the input scan, so the keyboard is not read and
    /// the computer is not called. `cpu` is always `[None, None]` here.
    pub demo: Option<Vec<u16>>,
    /// **Stage 18g** — the PC-speaker sequencer (`CS:4196`, see [`crate::sound`]): the cue queue,
    /// the note streams and the envelope. It is ticked once per engine tick, because the original
    /// drives it from `INT 1Ch` on the 240 Hz PIT, not from the frame.
    pub sound: sound::Sequencer,
    /// What the sequencer wrote to the ports on the last tick: `div` -> PIT channel 2, `gate` ->
    /// port 0x61. The window's square-wave generator and `--sound-dump` both read this.
    pub sound_out: sound::Out,
    /// The keys the computer produced on the last frame, per spy (for the traces and the report).
    pub last_cpu_keys: [Keys; 2],
    /// The object byte under each spy's feet at the computer's last decision (a trace aid).
    pub last_cpu_cell: [u8; 2],
    /// `DS:0x31BF` / 2 — the **difficulty** the options screen selected, 0..4 (`CS:1066` steps the
    /// word by 2). Written into the data segment as `2 * difficulty`; none of the tables it indexes
    /// (0x316C, 0x31CB, 0x31E1) are in a path this port models yet — see the stage 15 report.
    pub difficulty: u16,
}

impl Game {
    pub fn new(exe: Vec<u8>, opts: &RoundOpts) -> Result<Game, String> {
        let data = IslandData::from_exe(&exe, opts.island)?;
        let mut levels = data.levels;

        let mut ds = exe
            .get(DS_FILE_OFF..DS_FILE_OFF + 0x10000)
            .ok_or_else(|| "spy.exe too short for a 64 KB data segment".to_string())?
            .to_vec();
        // The image is the data segment as the *setup* screen left it: its object-block area is
        // filled with 0x02 (a valid object code, which paints 72 spurious sprites over the room —
        // see `round::materialise_round_start`) and its spy-bank words are zero, so no sprite can
        // be drawn at all. A real round writes those at round start; this does the same.
        round::materialise_round_start(&mut ds);

        let terrain = Terrain::from_exe(&exe)?;

        // ---- the island's objects, then the spies, then the hatch ----------------------------
        //
        // `FUN_26c0_257b`'s order, and the order matters twice: the object placement tests the
        // *pre-flip* level table, and the hatch's cell comes out of the draws the spies left over.
        let ds_img = exe
            .get(DS_FILE_OFF..DS_FILE_OFF + 0x10000)
            .ok_or_else(|| "spy.exe too short for a 64 KB data segment".to_string())?
            .to_vec();
        let mut obj_rng = match opts.obj_state {
            Some(s) => round::ObjRng::at(s),
            // The original's normal play: `DX` from `int 21h ah=2Ch`, not the attract `0x770D`.
            None if opts.obj_clock => round::ObjRng::from_clock(opts.obj_clock_dx),
            None => round::ObjRng::from_seed(opts.seed),
        };
        let mut layout = round::place_round_objects(
            &mut ds,
            &ds_img,
            &levels,
            &terrain,
            data.objects_unknown as usize,
            &mut obj_rng,
        );

        let kit_word = data.kit;
        let blank_spy = |i: usize| SpyState {
            x3: 0,
            y3: 0,
            screen: 0,
            dir: START_DIR,
            frame: STAND_FRAME,
            frame_step: 0,
            vel_x: 0,
            vel_y: 0,
            set: 0x24,
            extra: round::SPY_POSE_EXTRA[i],
            pose_row_shift: false,
            pose_raised: false,
            pose_state: 0,
            pose_clip: 0,
            saved_set: 0x24,
            saved_dir: 0,
            launched: false,
            out_of_round: false,
            mirror: false,
            map_x: 0,
            blank: false,
            fire_prev: false,
            carried: CARRY_NONE,
            kit: kit_word,
            dyn_sticks: 0,
            case_owned: false,
            trap_open: false,
            trap_sign: 1,
            map_open: false,
            map_sign: 1,
            trail: [0; 5],
            kit_blink: 1,
            storm_phase: 1,
            cursor_delay: 0,
            // **stage 24a** — `DS:0x2F3C` (+2): the trapulator's cursor. The round start sets it from
            // the image's own word (`CS:125F` copies `0x2FB2..` over `0x2EB8..`, which covers it),
            // and the image holds **12** — the **map slot**. That is what the recording shows: the
            // attract round's first dump reads `0x2F3C = 0x2F3E = 12`, spy 2's panel opens with the
            // cursor still on 12 and his A press closes the panel *and opens the map* (`CS:2A82:
            // cmp $0xC,%bx; 0x2F48 = 1; goto 0x2B6E`), then the next A closes the map. Starting the
            // port on 0 instead left spy 2 pressing A on an empty kit slot (buzz, panel stays up)
            // for the whole demo, which is why his walk diverged at frame 37.
            trap_cursor: items::SLOT_MAP,
            act_latch: false,
            fire_taps: 0,
            act_state: 0,
            anim: None,
            ball: None,
            icicle_latch: false,
            icicle_hit: false,
            dir_prev: [false; 4],
            cursor_req: 0,
            blocked: false,
            refused: false,
            heat: HEAT_FULL,
            terrain_t: 0,
            base_t: 0,
            frozen: 0,
        };
        let mut spies: [SpyState; 2] = std::array::from_fn(blank_spy);

        // ---- the round-start placement, exactly as `FUN_26c0_257b` does it -------------------
        //
        // The loop places player 2 and then player 1. For each: draw `screen = rand & 6`,
        // `x = 26 + (rand & 0xFF)`, `y = 44`; accept only when the terrain code there is 0 (open
        // ground); for player 1 (the last one placed) also refuse player 2's screen. What the port
        // used to do — always x = 29 on the island's start screen — was reading the *uninitialised
        // data-segment image* (x1 = 29, x2 = 92 before any round starts), which on island 1 is the
        // level-8 band west of the room. That is the "spies start in the ocean" defect.
        let mut placed_screen: Option<u16> = None;
        for slot in [1usize, 0] {
            let p = &mut spies[slot];
            // **`CS:257b` does not test `DS:0x31dd`.** It places spy 2 and *then* spy 1 whatever the
            // round's player count is; the one-player word only decides who drives spy 2 afterwards.
            // The port used to skip spy 2's two draws here, which shifted every later draw — the
            // hatch and the fire lottery — so a one-player round's hatch differed from the
            // two-player round of the same seed (island 1, seed 5: band 4 entry 44 vs band 6 entry
            // 83). Spy 2 is blanked *after* the placement instead, below.
            // An explicit start (tests, the recorded walk): honour it and skip the draw.
            if opts.start_screen.is_some() || opts.start_x.is_some() || opts.start_x2.is_some() {
                let screen = opts.start_screen.unwrap_or(if slot == 0 {
                    data.start_screen_p1
                } else {
                    data.start_screen_p2
                });
                let want = if slot == 0 { opts.start_x } else { opts.start_x2.or(opts.start_x) };
                let x = want.unwrap_or(START_X_BASE + 12).clamp(terrain::X_MIN, terrain::X_MAX);
                p.screen = screen;
                p.x3 = x * SUB;
                p.y3 = opts.start_y.unwrap_or(START_Y).clamp(Y_MIN, Y_MAX) * SUB;
                p.map_x = (x - MAP_X_ON_ENTRY_OFFSET).clamp(0, MAP_X_MAX);
                p.dir = START_DIR;
                p.frame = STAND_FRAME;
                p.extra = round::SPY_POSE_EXTRA[slot];
                placed_screen = placed_screen.or(Some(screen));
                continue;
            }
            // a hard cap so a table with no open ground can never hang the engine
            let mut tries = 0;
            loop {
                tries += 1;
                let screen = (obj_rng.byte() as u32 & START_SCREEN_MASK) as u16;
                // **stage 24a** — `CS:25F8..0x2601`: for the *second* spy placed (player 1) a screen
                // equal to the first spy's is refused, and the refusal jumps back to `0x25F2`, the
                // **screen** draw alone — the x draw at `0x2605` has not happened yet. Only the
                // terrain refusal (`0x2618`, after both draws) costs two draws. Burning the x here
                // too shifts every later draw by one: on the attract round it put spy 1 at x 179
                // instead of the recorded 112 and moved the hatch from band 4 entry 57 to band 6
                // entry 83 (recorded: `m1-fix-960` ds_00003 / `playtest3` ds_00246).
                if slot == 0 && placed_screen == Some(screen) {
                    continue;
                }
                let x = START_X_BASE + obj_rng.byte() as i32;
                if terrain.start_ok(&levels, screen, x, START_Y) {
                    p.screen = screen;
                    p.x3 = x * SUB;
                    p.map_x = (x - MAP_X_ON_ENTRY_OFFSET).clamp(0, MAP_X_MAX);
                    break;
                }
                if tries > 4096 {
                    // No open ground anywhere on the drawn screens: fall back to the island's
                    // record start screen and the first open cell of its row, and say so.
                    let screen = if slot == 0 { data.start_screen_p1 } else { data.start_screen_p2 };
                    let row = (screen / 2) as usize;
                    let mut found = None;
                    for col in 0..12 {
                        for off in 1..25 {
                            let x = terrain.gate(col) + off;
                            if terrain.start_ok(&levels, screen, x, START_Y) {
                                found = Some(x);
                                break;
                            }
                        }
                        if found.is_some() {
                            break;
                        }
                    }
                    let _ = row;
                    // a fallback that changes nothing a player would notice: diagnostic, `-v` only
                    crate::note_err!("antics: no random start on island {} — falling back to x={:?}",
                              data.island + 1, found);
                    p.screen = screen;
                    let x = found.unwrap_or(START_X_BASE + 12);
                    p.x3 = x * SUB;
                    p.map_x = (x - MAP_X_ON_ENTRY_OFFSET).clamp(0, MAP_X_MAX);
                    break;
                }
            }
            p.y3 = START_Y * SUB;
            p.dir = START_DIR;
            p.frame = STAND_FRAME;
            p.frame_step = 0;
            p.set = 0x24;
            p.extra = round::SPY_POSE_EXTRA[slot];
            placed_screen = Some(p.screen);
        }

        // **The blanking, after the placement** (`CS:257b` places both spies and nothing else
        // decides that). Spy 2's draws are kept — they are part of the stream the hatch and the fire
        // lottery read — but its record is reset to the blank one a one-player round starts with, so
        // nothing downstream can see a spy the round does not have. The reset values are exactly the
        // ones the old `continue` left behind, so a one-player round's *rendering* is unchanged; only
        // the layout stops diverging from the two-player one.
        if opts.players < 2 {
            spies[1] = blank_spy(1);
        }

        // The hatch comes after the spies (it draws from the same stream), then the level-table
        // rigging — the original's order, so the flip sees the same table the placement did.
        // `armed` is the launch-site option: HIDE leaves the three bytes unwritten (`CS:2696`),
        // which is what makes the hatch invisible until the briefcase is full.
        layout.hatch = round::place_hatch(&mut ds, &ds_img, &levels, &terrain, &mut obj_rng, opts.launch);

        // **read + measured** — `CS:26ae..0x26f3`: one descending pass over the 60 cells that pair
        // each outdoor cell with the interior five rows below it. An interior whose level is 0x12
        // (the bare dome plate) becomes 0x16 (the dome plate **with a flame**) with probability
        // 0xA1/0x100, and the level is re-rolled if no interior got one; a level-2 outdoor cell
        // becomes the 0x18 igloo dome one time in four. The fire therefore lives in the level
        // table, not in a sprite — see `round::place_igloo_fires`. The port used to leave every
        // interior at 0x12 ("all igloos have no fire") and to run the door flip in its own
        // row-major order; the attract round replays this loop exactly.
        let fires = round::place_igloo_fires(&mut levels, &mut obj_rng);

        // `--start-hatch`: stand spy 1 on the hatch's own cell, no draws consumed (`CS:2436`'s
        // landing coordinates: x = 2*entry + 2, y = 0x36).
        if opts.start_hatch {
            if let Some(h) = &layout.hatch {
                let x = 2 * h.entry as i32 + 2;
                spies[0].screen = h.band;
                spies[0].x3 = x * SUB;
                spies[0].y3 = 0x36 * SUB;
                spies[0].map_x = (x - MAP_X_ON_ENTRY_OFFSET).clamp(0, MAP_X_MAX);
            }
        }
        // `DS:0x31B6` for the live round: the hatch's middle entry, resolved to a segment address.
        let hatch_addr = layout.hatch.as_ref().map(|h| {
            let base = round::block_addr(&ds_img, h.band).unwrap_or(0);
            base + round::OBJ_ROW_LEN * h.row + h.entry
        });
        // `--carry` / `--carry2` (demo aid): the same state a trapulator take leaves behind.
        for (i, code) in [opts.carry, opts.carry2].into_iter().enumerate() {
            if let Some(code) = code {
                if i < spies.len() {
                    spies[i].carried = code;
                    if code == 12 {
                        spies[i].kit |= items::KIT_CASE_BIT;
                    }
                    spies[i].set = items::code_pose(
                        &exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000], code);
                }
            }
        }
        let clock_secs = opts.clock_override.unwrap_or(data.clock_secs);
        let links = cpu::links_from_layout(&layout);
        // the sequencer's only initial state is `DS:0x3249`, the sound-enable word (1 in the image)
        let sound = sound::Sequencer::new(&ds);
        let mut g = Game {
            links,
            clock_left: clock_secs,
            clock_word: 0,
            clock_secs_total: clock_secs,
            data,
            levels,
            spies,
            layout,
            players: if opts.demo.is_some() { 2 } else { opts.players.clamp(1, 2) },
            ticks: 0,
            frames: 0,
            frame_ticks: opts.frame_ticks.unwrap_or(FRAME_TICKS).max(1),
            phase: Phase::Playing,
            objects: opts.objects,
            spies_layer: opts.spies,
            void_solid: opts.void_solid,
            igloo_entry: opts.igloo_entry,
            visitor_draw: opts.visitor_draw,
            storm_off: opts.no_storm,
            storm_drawn: [false; 2],
            visitor_eval_mover_only: opts.visitor_eval_mover_only,
            spy_block: opts.spy_block,
            terrain,
            frame_phase: 0,
            pending: [Keys::default(); 2],
            exe,
            ds,
            assets: round::RoundAssets::load(),
            mask: MASK_BASE,
            part_owner: [items::OWNER_NONE; items::PART_OWNERS],
            items_ui: opts.items_ui,
            item_counts: [0; 11],
            carry_at: opts.carry_at.clone(),
            hatch: HatchState {
                armed: if opts.launch { 2 } else { 0 },
                addr: hatch_addr,
                ..HatchState::default()
            },
            fires,
            rocket: false,
            rocket_asc: 0,
            rocket_x: 0,
            over_flag: false,
            end: None,
            end_tick: 0,
            difficulty: opts.difficulty.min(4),
            // `DS:0x31DD == 1` — a one-player round hands spy 2 to the computer (`CS:0348`). The
            // caller decides that (the CLI and the setup screen both set `cpu[1]`); a round always
            // has both spies in it, because the computer's spy is one of them.
            cpu: [
                if opts.demo.is_some() {
                    // `CS:0325`'s attract branch returns before the `CS:129C` call: neither spy is
                    // ever the computer in a demo round, whatever `--cpu1`/`--cpu2` said.
                    None
                } else if opts.cpu[0] {
                    Some(cpu::Cpu::new(0, opts.difficulty))
                } else {
                    None
                },
                if opts.demo.is_some() {
                    None
                } else if opts.cpu[1] {
                    Some(cpu::Cpu::new(1, opts.difficulty))
                } else {
                    None
                },
            ],
            demo: opts.demo.clone(),
            sound,
            sound_out: sound::Out::default(),
            last_cpu_keys: [Keys::default(); 2],
            last_cpu_cell: [0; 2],
            rng: Rng::new(opts.seed ^ 0x14_0F_EE),
            placed_rng_state: obj_rng.state(),
            placed_rng_draws: obj_rng.draws,
            stand_rng: Rng::new(opts.seed ^ 0x23_0C_5A),
            stand_roll: None,
            body_ran: [false; 2],
            seed_pending: [false; 2],
            feet_addr: [None; 2],
            feet_prev: [None; 2],
        };
        if let Some(h) = opts.heat {
            // demo aid only: `DS:0x2F58`/`0x2F5A` are otherwise written by `21bd` alone
            g.spies[0].heat = h.min(HEAT_MAX);
            g.spies[1].heat = h.min(HEAT_MAX);
        }
        if let Some(h) = opts.heat2 {
            g.spies[1].heat = h.min(HEAT_MAX);
        }
        if opts.parts {
            for s in g.spies.iter_mut() {
                s.kit |= round::KIT_ALL_PARTS;
            }
        }
        // **stage 18c — the map's plate and trails, at round start.**
        // `CS:1204` copies the island's own 992-byte plate blob into `DS:0x1052` (this *is* the
        // map's composition: seven blobs in the image, one per island, 0x3E0 apart), and `CS:26F7`
        // fills both spies' five-entry trails with their starting cell.
        round::materialise_map_plate(&mut g.ds, &ds_img, g.data.island);
        for i in 0..2usize {
            let pos = g.map_pos(i);
            g.spies[i].trail = [pos; 5];
        }
        g.blank_idle_console();
        Ok(g)
    }

    /// The live data segment (objects, spies, item words). Read-only: the report and the demos
    /// print it, nothing outside the engine writes it.
    pub fn ds_ref(&self) -> &[u8] {
        &self.ds
    }

    pub fn clock_secs(&self) -> u32 {
        // `DS:0x324D` is the displayed second itself; there is no tick arithmetic left to do.
        self.clock_left as u32
    }

    /// The frame's length in seconds, from the frame's own tick count — one place, so a sweep
    /// (`--fps`) moves the clock, the heat, the ascent and the wall-clock report together.
    pub fn frame_secs(&self) -> f64 {
        self.frame_ticks as f64 / TICK_HZ as f64
    }

    /// The level word for a screen and world column: `word[0x24BE + 12*screen + 2*col]`, i.e. row
    /// `screen / 2` of the **10-row** table. Rows 5..9 are the igloo interiors (screen 10..18), so
    /// they are real entries, not a special case.
    pub fn level_at(&self, screen: u16, col: usize) -> Option<u16> {
        if screen % 2 != 0 {
            return None;
        }
        let row = (screen / 2) as usize;
        if row >= self.levels.len() {
            return None;
        }
        Some(self.levels[row][col.min(11)])
    }

    pub fn in_interior(&self, player: usize) -> bool {
        self.spies[player].screen >= FIRST_INTERIOR_SCREEN
    }

    /// One tick of game time. `input` is what the keyboard (or a script) is holding now. Returns
    /// **true when that tick completed a game frame** — the window uses it to present once a frame
    /// (the original draws once a frame) instead of once a tick.
    ///
    /// The engine's tick is the original's timer interrupt: 240 ticks a second (`TICK_HZ`, PIT
    /// divisor 4972). A game frame is [`FRAME_TICKS`] = 26 of them, 108.34 ms, and **everything**
    /// except input sampling happens there: the clock's count (`CS:0680`), heat, the ascent, the
    /// drawing. A script still counts in ticks, so one frame of input is 26 lines.
    pub fn tick(&mut self, input: Input) -> bool {
        self.ticks += 1;
        // **stage 18g** — `INT 1Ch` runs on the hardware timer, not on the game's frame: one tick of
        // this loop is one sequencer tick (`sound::SEQUENCER_HZ`). Ticking it here, before anything
        // else, keeps a cue's internal note lengths independent of how long a frame takes.
        self.sound_out = self.sound.tick(&self.ds);
        // **demo aid** (`--carry-at`): the hand changes at the given tick, as a trapulator take
        // would leave it. Never used by the engine's own logic.
        if !self.carry_at.is_empty() {
            // in **frames** (stage 16a): the aid's unit follows the engine's, so a demo's number
            // means the same thing at any `--fps`
            let due: Vec<(usize, u16)> = self
                .carry_at
                .iter()
                .filter(|(_, t, _)| *t <= self.frames as u32)
                .map(|(p, _, c)| (*p, *c))
                .collect();
            self.carry_at.retain(|(_, t, _)| *t > self.frames as u32);
            for (p, code) in due {
                self.set_carried(p, code);
                self.set_pose(p, code);
            }
        }
        if self.phase != Phase::Playing {
            return false;
        }
        // **the frame latch.** The original reads the keyboard through per-key *flags* that the
        // handler sets on the press (`DS:0x3287`/`DS:0x328F`/... are pointers to them) and that only
        // the frame code clears when it consumes one. This port samples the keyboard every tick but
        // runs the spies a frame at a time, so a press that starts and ends between two game
        // frames would otherwise vanish: the owner's "sometimes I have to press it twice or hold the
        // button". Taking the union of every tick in the frame is the same thing as the flag being
        // set at any point before the frame ran. See STAGE8B_REPORT.md §1. Stage 16a moved the tick
        // from 60 Hz to the original's 240, so a frame now takes 26 samples instead of 6 — the latch
        // itself is unchanged and the guard is
        // `a_tap_on_the_last_tick_of_a_frame_still_moves_the_spy`.
        // A spy the computer plays ignores the keyboard for that frame entirely: the original's
        // input scan does not copy that player's keys at all (`CS:0348`) and calls `CS:129c`
        // instead. The substitution happens on the frame, not the tick, because the AI runs once
        // per game frame — exactly where `CS:0325` sits in the original's loop.
        self.pending[0] = if self.cpu[0].is_some() { Keys::default() } else { self.pending[0].or(input.p1) };
        self.pending[1] = if self.cpu[1].is_some() { Keys::default() } else { self.pending[1].or(input.p2) };
        self.frame_phase += 1;
        if self.frame_phase < self.frame_ticks {
            self.round_end_check();
            return false;
        }
        self.frame_phase = 0;
        self.frames += 1;
        {
            // **stage 23c — the storm's flip** (`CS:0B9E`). The original draws with `W` and then
            // stores `-W`, so the value a frame renders with is the negation of the previous
            // frame's. This port renders *after* the frame, so the negation is taken here, at the
            // top of the next frame, using what the previous draw actually did (`storm_drawn`).
            // It sits outside `step_frame` on purpose: the draw loop runs for every spy in every
            // state — launched, frozen, animating — so the snow must keep twinkling on a statue.
            for i in 0..2usize {
                if self.storm_drawn[i] {
                    self.spies[i].storm_phase = -self.spies[i].storm_phase;
                }
            }
            // `CS:065e`..`CS:068e` — the clock, **once a frame**: the tick word takes this frame's
            // ticks and is compared with 0xDC; when it is above, it is cleared and `FUN_26c0_229e`
            // takes one count off `DS:0x324D`. Eight frames (208 ticks) never reach it and nine
            // (234) always do, so a count is 9 frames = 975.1 ms — the original's clock is 2.5 %
            // fast against a wall-clock second, and that is what the recordings show (975.0 ms).
            if self.clock_left > 0 {
                self.clock_word += self.frame_ticks as u16;
                if self.clock_word > CLOCK_TICKS_WORD {
                    self.clock_word = 0;
                    self.clock_left -= 1;
                }
            }
            // `CS:0610` — the **reveal**, at the top of the frame loop and before the players:
            // `if (word[0x31D5] == 0 && word[0x2EB8] == 0x6C0) { 15 0F 15; word[0x31D5] = 2 }`.
            // With the launch site on HIDE the hatch is not in the block at all, so this is the
            // frame on which it appears — the owner's "until all parts are in the briefcase".
            self.hatch_reveal();
            // The original's per-player loop starts at player 2 and steps down (`uVar12 = 2; do {
            // ... } while (-1 < (int)(uVar12 -= 2))`), so player 2's frame runs first. Which order
            // matters only when both spies are eligible for the visitor flag on the same frame:
            // the first one tested takes it and the second sees `other.flag != 0` and does not.
            let order: [usize; 2] = if self.players > 1 { [1, 0] } else { [0, 1] };
            let mut keys = [self.pending[0], self.pending[1]];
            self.pending = [Keys::default(); 2];
            // **stage 17** — the computer's own input, computed once per frame in the original's
            // own place (`CS:0325`, before the per-player loop, `SI` = the spy it drives).
            // **stage 24a** — in a demo round `CS:0325` takes its attract branch instead and
            // neither the computer nor the keyboard is consulted: the frame's keys are the
            // recorded mask, and the keys the keyboard latched during the frame's 26 ticks are
            // dropped on the floor exactly as `CS:0203`'s twelve `0` stores would.
            if self.demo.is_some() {
                let mask = self.demo_mask(self.frames as usize - 1);

                let (k1, k2) = demo_keys(mask);
                keys = [k1, k2];
            } else {
                self.cpu_frame(&mut keys);
            }
            // **stage 24d — the snowball pass is per player, not per frame.** `CS:303c` steps a
            // ball and resolves its hit (`CS:307c..312d`), and a hit starts the victim's knock-out
            // in the *same* frame it lands (`CS:277c` seeds `0x2EFC = -2` and the victim's own
            // `CS:098c` advances it to 0). Stage 16b read the call site (`CS:e6a`, inside the draw
            // at `CS:c93`) as "after both frame bodies" and ran this pass once, after the loop.
            // The recordings say otherwise: with the pass after both bodies the duel's knock-out
            // chain runs a 26-frame cycle and is 2-3 frames behind the demo by frame 180; with it
            // inside the per-player loop — so the ball of the player who runs first lands before
            // the other player's body — the chain is the recording's 25-frame one and every
            // positional field matches for the whole recorded window (stage 24d §3). The two
            // console draws are not symmetric either (`CS:e78..e88` redraws at `CS:cb4` for the
            // *other* spy when the visitor flags say so), which is how a per-player effective
            // order can come out of a draw that is called once per console.
            self.body_ran = [false; 2];
            // `CS:0afd` — `FUN_26c0_2549` refreshes each spy's cached feet cell (`DS:0x32D7`) at the
            // **end** of that spy's own body, so every reader inside a body sees the cell the spy
            // stood on at the end of the *previous* frame. The port keeps the same two words (see
            // [`Game::feet_addr`]) and snapshots last frame's value here, before any body moves.
            self.feet_prev = self.feet_addr;
            for i in 0..2usize {
                if self.feet_addr[i].is_none() {
                    // the round start's own `2549` (`CS:2639`): the first frame has no previous body
                    self.feet_addr[i] = self.cell_of(i);
                    self.feet_prev[i] = self.feet_addr[i];
                }
            }
            for &i in order.iter().take(self.players) {
                self.step_frame(i, keys);
                self.body_ran[i] = true;
                self.feet_addr[i] = self.cell_of(i);
            }
            // **stage 24f — the ball pass is where `CS:e6a` puts it: in the draw, one console at a
            // time, *after* both frame bodies.** Stage 24d moved it into the per-player loop because
            // a knock-out that player 2's ball lands on player 1 then came out a frame older than
            // player 2's — but the code's own reason for that asymmetry is the draw's clamp at
            // `CS:0d44` (see [`Game::spend_seed`]): player 2's console clamps *and* steps its ball,
            // and only then does player 1's console clamp him, so a ball of player 2's that lands on
            // player 1 is spent to frame 0 in the same frame while one of player 1's on player 2 is
            // not. Keeping the pass inside the loop instead made every ball land *before* the
            // victim's own step, and the demo measures the difference: at frame 694 the recording's
            // spy 1 takes his fourth `down` step (y 54 → 56) and the ball thrown at 689 — row 30,
            // column 73 — misses him by the hit box's own rule (`[y-24, y-14]`), while the port hit
            // him at y 54 and froze him for three frames.
            for &i in order.iter().take(self.players) {
                self.draw_clamp(i);
                self.snowball_step(i);
            }
            // `CS:097f` — `call 0x2392` runs after the move on the per-player path, for both
            // spies. Doing it here, once per spy after both have moved, is the same thing except
            // when the two arrive on the hatch on the *same* frame: there the original's
            // "the other spy is already in the animation" guard (`CS:2404`) settles it, and so
            // does this loop's first-wins order.
            for &i in order.iter().take(self.players) {
                self.hatch_touch(i);
            }
            self.hatch_advance();
            // `CS:0afd` runs for the hatch's own spy after his scripted step too (his body ends at the
            // script, not at `0xa1a`).
            for i in 0..2usize {
                if self.hatch.spy == i || self.hatch.other == Some(i) {
                    self.feet_addr[i] = self.cell_of(i);
                }
            }
            // `CS:20f2` — the ascent is one step a frame, and the frame is where the original
            // advances it too (its `0xc93` draws exactly once per frame).
            self.ascent_tick();
            // **stage 18c** — the map's two per-frame words, after the movement and before the
            // draw, which is where `CS:36D1` (the trail shift) and `CS:2B85`/`CS:2B9E` (the map
            // taking itself down) sit in the original's frame: both run before `CS:0BAA` draws.
            for &i in order.iter().take(self.players) {
                self.map_trail_step(i);
                if self.spies[i].map_open && self.map_hidden(i) {
                    self.spies[i].map_open = false;
                }
            }
            // **stage 23c** — record what this frame's draw will do, for the next frame's flip.
            // The draw runs immediately after the frame body, so this is the draw's own eligibility.
            for i in 0..2usize {
                self.storm_drawn[i] = self.storm_active(i);
            }
        }
        // **stage 15** — the round-end exits (`CS:0649`, `CS:065e`, `CS:06c0`)
        self.round_end_check();
        true
    }

    /// **Stage 23a** — a key that was already down when the round began is not a press.
    ///
    /// The original's buttons are *flags*: the `INT 9` key-down handler sets them
    /// (`DS:0x327F` = A, `DS:0x3283` = B, ...) and the frame code clears the one it consumes, so a
    /// key held down produces exactly **one** press. The options screen's GO! is Return — and the
    /// measured keyboard map gives P1 Return as the fire key (stage 7 §2.3) — so that one press is
    /// consumed by the screen that started the round, and `0x440`/`0x257b`/`0x60d` begin with no
    /// pending press at all even though the key is still down.
    ///
    /// The port samples the keyboard as a **level** every tick and takes the edge against
    /// [`SpyState::fire_prev`], which starts `false`. Without this call the GO! press arrives again
    /// as a fire press on the round's first frame: `FUN_26c0_2d22` -> `FUN_26c0_373b` then takes
    /// whatever the placement left on the spy's own cell, which is the owner's "the white spy
    /// started with the fuel tank IN HAND" (stage 23a: 32 of 560 clock-seeded layouts, 5.7 %).
    /// The caller hands in the keys as they are **at the moment the round is created**.
    ///
    /// Headless paths never call it: a script's first tick is its own input, and the recorded
    /// replays stay byte-identical.
    pub fn absorb_held_keys(&mut self, input: Input) {
        for (i, k) in [input.p1, input.p2].into_iter().enumerate() {
            self.spies[i].fire_prev = k.fire;
            self.spies[i].dir_prev = [k.up, k.down, k.left, k.right];
        }
        self.pending = [Keys::default(); 2];
    }

    /// **Stage 24a** — is this round the attract demo? (`CS:0325`'s `DS:0x3203` branch.)
    pub fn is_demo(&self) -> bool {
        self.demo.is_some()
    }

    /// The recorded mask for a **0-based frame** of the demo: `DEMO.DAT`'s word *n*.
    ///
    /// Past the end of the stream the original keeps reading — `CS:0203` never rewinds `DS:0x0FB6`
    /// and nothing bounds it — so it reads whatever the load left in the data segment above
    /// `DS:0x000D + 1882`, which is zero in both recorded demos (`m1-fix-960`, `playtest3`). This
    /// returns 0 there, i.e. both spies hold nothing, which is what those dumps show.
    pub fn demo_mask(&self, frame: usize) -> u16 {
        self.demo
            .as_ref()
            .and_then(|s| s.get(frame).copied())
            .unwrap_or(0)
    }

    /// How many frames the recorded stream covers (0 for an ordinary round).
    pub fn demo_frames(&self) -> usize {
        self.demo.as_ref().map(|s| s.len()).unwrap_or(0)
    }

    /// **Stage 17** — one frame of the computer's input (`CS:0325` -> `CS:129c`).
    ///
    /// The AI is a pure function of a snapshot of the round: [`cpu::World::from_game`] copies out
    /// what the routine reads, [`cpu::Cpu::think`] returns the six key flags a keyboard would have
    /// produced plus the two words it writes directly, and this applies both. Nothing else in the
    /// engine knows whether a spy is a computer: every rule downstream — the walk, the doors, the
    /// trapulator, the throw, the hazards — reads the same keys a human's press produces.
    /// The computer's input for one frame, for the probes: identical to what a tick's frame
    /// boundary does, but callable on a round seeded from a recorded dump.
    pub fn cpu_input(&mut self, i: usize) -> Option<Keys> {
        if self.cpu.get(i)?.is_none() {
            return None;
        }
        let mut keys = [Keys::default(); 2];
        self.cpu_frame(&mut keys);
        Some(keys[i])
    }

    /// **test only** — put a spy on a cell and write an object byte there, so the AI's view of the
    /// live segment can be checked without a recording.
    #[cfg(test)]
    pub fn test_put(&mut self, i: usize, x: i32, y: i32, code: u8) -> usize {
        self.spies[i].x3 = x * SUB;
        self.spies[i].y3 = y * SUB;
        let addr = items::cell_addr(&self.ds, self.spies[i].screen, x, y).expect("cell");
        self.ds[addr] = code;
        addr
    }

    /// The AI's own words (the probes print them against a recorded dump's).
    pub fn cpu_state(&self, i: usize) -> Option<&cpu::Cpu> {
        self.cpu.get(i)?.as_ref()
    }

    fn cpu_frame(&mut self, keys: &mut [Keys; 2]) {
        for i in 0..2 {
            if i >= self.players || self.cpu[i].is_none() {
                continue;
            }
            let mut c = self.cpu[i].take().expect("checked");
            let (k, side, cell) = {
                let world = cpu::World::from_game(self, i);
                let mut rng = std::mem::replace(&mut self.rng, Rng::new(0));
                let out = c.think(&world, &mut rng);
                self.rng = rng;
                (out.0, out.1, world.cell[i])
            };
            self.cpu[i] = Some(c);
            if let Some(v) = side.trap_open {
                self.spies[i].trap_open = v;
            }
            // **stage 20c** — the slot the errand asked for, written into the cursor word on the
            // frame the panel opens (the stow's own write, `CS:31aa`, is the same word).
            if let Some(v) = side.trap_cursor {
                self.spies[i].trap_cursor = v;
                // **stage 20c** — the request that steps the cursor off the cell before the panel
                // opens (`CS:1bcc`, the same RIGHT flag) must not survive into the panel: the
                // trapulator serves a latched request on the frame it opens and would walk the
                // cursor off the slot the errand just placed it on. The panel's own repeat
                // counter starts clear for the same reason (`CS:2b04` only reloads it on a move).
                self.spies[i].cursor_req = 0;
                self.spies[i].cursor_delay = 0;
            }
            if let Some(v) = side.act_latch {
                self.spies[i].act_latch = v;
            }
            if side.stow {
                self.stow_or_drop(i);
            }
            // **stage 18f** — the computer's keys are **pulses, not levels**. `CS:129c`'s own head
            // (`12a0`..`12c8`) clears all six of that spy's key flags through their pointer table
            // and then sets whatever it wants *this* frame, so a behaviour that holds button B
            // (the punch, `CS:1a46`) re-presses it on every frame it runs. The engine above reads
            // the level and derives the edges against the previous frame, so for a CPU-driven spy
            // the previous frame's level has to be "nothing down" — otherwise frame two of a held
            // B is a *walk* and the punch is thrown exactly once.
            //
            // The human's path is not the same code and is not pulsed here: see the report,
            // §2 — the keyboard's flags are set on the key-down by the interrupt handler and
            // cleared by whoever consumes them (`12a0` is inside the AI, not the frame loop).
            self.spies[i].fire_prev = false;
            self.last_cpu_keys[i] = k;
            self.last_cpu_cell[i] = cell;
            keys[i] = k;
        }
    }

    /// The three ways the original's round loop ends — the exits at `CS:0649`, `CS:065e` and
    /// `CS:06c0`, in the order the loop reaches them:
    ///
    /// ```text
    /// 065e  if (word[0x324d] <= 0) {
    ///         if (0x2f18 + 0x2f1a) goto draw     ; a spy is animating: keep drawing
    ///         if (0x2f70 + 0x2f72) goto draw     ; a rocket is flying: keep drawing
    ///         goto 0x6d8                         ; THE CLOCK'S EXIT
    ///       }
    /// 0649  if (word[0x248a] != 0) { word[0x248a] = 0; goto 0x60d }   ; THE ASCENT'S EXIT
    /// 06c0  if (0x2f64 >= 2 && 0x2f66 >= 2) goto 0x60d                ; BOTH OUT OF THE ROUND
    /// ```
    ///
    /// `0x60d` is the top of the outer loop: `call 0x440` resets the round (`0x46f`), then `0xed1`
    /// is the **options screen** and only then does `0x125f` load an island and start the clock
    /// again. So all three exits land on the options screen with no winner and no score, which is
    /// what the recordings show: `m4h`/`m4j`'s one-frame cut, `playtest3`'s two `0x324d` = 0 dumps
    /// before the next round, and `freeze-death`'s frame 14,139 (options panel, clock 00:00).
    ///
    /// The port tests these once a tick rather than once a frame; the words they read only move on
    /// frames, so the only difference is that a clock deadline falling between two frames is seen
    /// up to five ticks (83 ms) earlier than the original would see it.
    fn round_end_check(&mut self) {
        if self.phase != Phase::Playing {
            return;
        }
        // both spies out of the round — the freeze is the only way there, and it is the *only*
        // exit that does not wait for anything
        if self.players > 1 && self.spies.iter().take(self.players)
            .all(|s| s.frozen >= 2 || s.out_of_round)
        {
            self.end_round(RoundEnd::BothOut);
            return;
        }
        // the ascent's end (`0x248a`)
        if self.over_flag {
            self.end_round(RoundEnd::Launch);
            return;
        }
        // the clock at zero, once nothing is animating and no rocket is flying
        let animating = self.spies.iter().take(self.players).any(|s| s.pose_raised);
        if self.clock_left == 0 && !animating && !self.rocket {
            self.end_round(RoundEnd::Clock);
        }
    }

    fn end_round(&mut self, why: RoundEnd) {
        self.phase = Phase::Over;
        self.end = Some(why);
        self.end_tick = self.ticks;
    }

    /// A cue write, exactly as the game does it: `cmpw $2,0x34A0 / jg` then the two stores
    /// (`sound.rs`, `STAGE18B_RECON` §2.1). Returns `false` when the write was **dropped** because
    /// a cue was already playing — the original has no queue, so a busy frame is a silent one.
    fn cue(&mut self, ptr: u16) -> bool {
        self.sound.cue(ptr)
    }

    /// The cue that is playing, or the one the last frame queued and the next tick will start.
    ///
    /// State 3 is the handler's one-tick "take the pending cue" phase, during which `0x3494` still
    /// holds the previous pointer and `0x3496` holds the new one.
    pub fn cue_playing(&self) -> Option<u16> {
        match self.sound.state {
            3 => Some(self.sound.pending),
            s if s >= 4 => Some(self.sound.playing),
            _ => None,
        }
    }

    /// `CS:20f2` — the ascent, one step a frame. The original counts this in `FUN_26c0_20ee`, which
    /// `0xc93` calls from the **drawing** pass; the port counts it here because the drawing must not
    /// advance state (a redraw would otherwise skip a frame). Exactly one console drives it in the
    /// original — the launcher's own — so it is one `+2` a frame, not two.
    ///
    /// `CS:21ae`: at `0xee` the round-over flag `0x248a` goes up.
    fn ascent_tick(&mut self) {
        if !self.rocket || self.over_flag {
            return;
        }
        self.rocket_asc = self.rocket_asc.saturating_add(2);
        if self.rocket_asc >= round::ASCENT_END {
            self.rocket_asc = round::ASCENT_END;
            self.over_flag = true;
        }
    }

    // (Stage 16a removed the empty `clock_tick` placeholder: the round clock now counts in the
    // frame block where `CS:0680` tests it, and there is no per-tick timer left. The snowstorm's
    // and the storm's own counters, when they come, are frame counters like every other.)

    /// `FUN_26c0_21bd` — **one heat change**, in the original's own unit: `AX` cold steps, each
    /// `2` on the one word, refused at the two ends (`CS:21d6` warming above 3, `CS:21f4` cooling
    /// past `0x3D`). The tail of the routine clears the base counter (`0x2F54`), so *any* terrain
    /// drain restarts the 138-frame clock — which is why the two rates never simply add.
    ///
    /// The original also redraws the bar here, twice over (`0x3455` composes the row art into the
    /// per-player bitmap at `0x330B`, `0x45b0` blits it to `0xB800` at `0x2F5C`). The port draws the
    /// bar from the state in [`Game::frame`], so nothing is owed here.
    fn heat_step(&mut self, i: usize, steps: i32) {
        let s = &mut self.spies[i];
        for _ in 0..steps.abs() {
            if steps < 0 {
                if s.heat <= HEAT_FREEZE_AT {
                    s.heat += 2;
                }
            } else if s.heat > HEAT_FULL {
                s.heat -= 2;
            }
        }
        s.base_t = 0;
    }

    /// `CS:073b..075e` — the per-frame heat block, before any key is read.
    ///
    /// ```text
    /// 73b  if (0x2F64) 0x3396                    ; a frozen spy is stopped every frame
    /// 745  if (0x2F58 > 0x3D) FUN_26c0_2238       ; the freeze itself
    /// 74f  0x2F54 += 1
    /// 753  if (0x2F54 < 0x8A) skip
    /// 75b  FUN_26c0_21bd(AX = -1)                 ; one step of cold, which clears 0x2F54
    /// ```
    fn heat_block(&mut self, i: usize) {
        if self.spies[i].frozen != 0 {
            self.spies[i].stop();
        }
        if self.spies[i].heat > HEAT_FREEZE_AT {
            self.freeze_death(i);
        }
        self.spies[i].base_t += 1;
        if self.spies[i].base_t >= HEAT_BASE_PERIOD {
            self.heat_step(i, -1);
        }
    }

    /// `FUN_26c0_2238` — **freezing to death**, exactly the code's ten writes.
    ///
    /// ```text
    /// 2238  if (0x2F18) return                       ; already animating: nothing
    /// 223f  3396                                     ; stopped
    /// 2243  other: 0x32CB = 0x0C; 277c               ; the survivor laughs
    /// 2251  315f                                     ; a tool into its slot, a part on the ice
    /// 2254  if (carried >= 9) 0x2327                 ; still holding a part: find it a cell
    /// 2269  0x2F20 = 0; 0x2F1C = 0; 0x2EF8 = 0x72    ; the frozen pose set, for good
    /// 227d  3396; 0x2F48 = 0; 0x2F18 = 0
    /// 228e  0x32CB = 0x14; 277c                      ; the death animation: state 0x14
    /// 2297  0x2F64 = 1                               ; frozen
    /// ```
    ///
    /// The pose set is written *before* `277c`, and `277c` saves the set it found into `0x32A7` —
    /// so the animation's "restore" restores `0x72`, which is the statue the recording keeps.
    fn freeze_death(&mut self, i: usize) {
        if self.spies[i].anim.is_some() || self.spies[i].pose_raised {
            return;
        }
        if self.spies[i].frozen >= 2 || self.spies[i].out_of_round || self.spies[i].launched {
            return;
        }
        self.spies[i].stop();
        // the survivor's laugh: `277c` refuses while he is animating, so this is the same test the
        // original's call makes
        let o = 1 - i;
        if self.spies[o].anim.is_none() && !self.spies[o].out_of_round {
            self.spy_laugh(o);
        }
        // the hands: `315f`, and then `2327` for a part that could not be dropped
        self.stow_or_drop(i);
        if self.spies[i].carried >= 9 {
            self.drop_part_anywhere(i);
        }
        self.spies[i].set = FREEZE_POSE_SET;
        self.spies[i].stop();
        self.spies[i].trap_open = false;
        self.spies[i].pose_raised = false;
        self.spies[i].act_state = round::STATE_FREEZE;
        self.start_anim(i, round::STATE_FREEZE, None);
        self.spies[i].frozen = 1;
    }

    /// `FUN_26c0_2327` — take a part out of the kit word and put it on the first free cell the
    /// object search gives (`FUN_26c0_272e`). Only the freeze reaches it.
    fn drop_part_anywhere(&mut self, i: usize) {
        let img = self.exe[DS_FILE_OFF..].to_vec();
        let mut found = None;
        for code in 8..=12u16 {
            let bit = items::code_bit(&img, code);
            if bit != 0 && self.spies[i].kit & bit != 0 {
                found = Some((code, bit));
                break;
            }
        }
        let Some((code, bit)) = found else { return };
        self.spies[i].kit &= !bit;
        self.spies[i].carried = CARRY_NONE;
        if let Some(cell) = self.free_cell_near(i) {
            self.ds[cell] = code as u8;
            self.link_record(i, code, cell);
        }
    }

    /// The first empty object cell from the spy's feet outwards — the same search the drop uses
    /// (`feet`, else one entry west, else one entry east). `None` when all three are taken, which
    /// the original answers by leaving the byte unwritten.
    fn free_cell_near(&self, i: usize) -> Option<usize> {
        let feet = self.cell_of(i)?;
        for c in [feet, feet.wrapping_sub(1), feet + 1] {
            if c < self.ds.len() && self.ds[c] == 0 {
                return Some(c);
            }
        }
        None
    }

    /// `FUN_26c0_1e0f` — **what the terrain under the spy does this frame**, dispatched on the code
    /// at the *projected* position (`x + 0x2F04`, `y + 0x2F00`), once per **pass through the frame
    /// body** — a *pixel step*, not a frame.
    ///
    /// | code | branch | effect |
    /// |---|---|---|
    /// | 0 | `CS:1e14` | the clip is zeroed |
    /// | 1, 0x17 | `CS:1ed9` | **water**: clip +2 a pass to `0x14`; every 12th pass `+2` cold |
    /// | 2 | `CS:1ea0` | **deep snow**: clip +2 to `6`; every **9th** pass `+2` cold (`CS:1ebb` is `jg 8`); **the snowshoes in hand skip the whole branch** (`CS:1e8b`) |
    /// | 3 | `CS:1e2d` | **ice**: while moving, a PRNG byte above `0xE6` (9.8 %) slips him: `-3` steps of cold, `315f`, state `0x10`, the other spy laughs |
    /// | 5 | `CS:1e22` | solid: `3396` |
    /// | 6 | `CS:1f08` | **the fire**: `3396` (it is solid) and every 2nd pass `+2` of heat |
    /// | 0xC, 0x1D | `CS:1ffa` | the igloo floor: every 21st pass `+2` of heat |
    /// | 0x16 | `CS:2014` | the clip is **left alone** |
    /// | everything else | `CS:2019` | the clip is zeroed |
    ///
    /// `0x2F60` is one counter shared by all four rate branches, so switching terrain mid-count
    /// carries the phase over — the port keeps that by using one field for it.
    ///
    /// **Stage 22a**: this runs once per *step*, because `CS:0944..0979` sits inside the body's
    /// loop and `CS:0a44`'s `jmp 0x7cd` sends a scrolling frame round again. A parked spy makes one
    /// pass a frame (12 frames a cold step in water); a spy walking with the view scrolling makes
    /// two (6 frames), which is what the recordings' `0x2F60` shows: it reads exactly 2 × `0x2F54`
    /// on every dump of `playtest5`'s water walk.
    fn terrain_tick(&mut self, i: usize) {
        let s = &self.spies[i];
        let (screen, x, y) = (s.screen, s.x() + s.vel_x, s.y() + s.vel_y);
        self.terrain_tick_at(i, screen, x, y);
    }

    /// The dispatch at an explicit cell. `CS:0976` passes `0x32C3`/`0x32C7` — the **candidate**
    /// words, which a door arm has not yet rewritten — so the pass that crosses a room probes the
    /// *old* row in the *new* room's mask; see [`Game::move_once`]. Every other caller probes the
    /// cell the spy is stepping onto, which is what [`Game::terrain_tick`] hands in.
    fn terrain_tick_at(&mut self, i: usize, screen: u16, x: i32, y: i32) {
        if self.spies[i].frozen >= 2 || self.spies[i].out_of_round || self.spies[i].launched {
            return;
        }
        if self.spies[i].anim.is_some() {
            // `CS:0979` runs before the animation branch at `CS:0982`, so an animating spy still
            // feels the terrain he is standing on
        }
        let s = &self.spies[i];
        let (vx, vy) = (s.vel_x, s.vel_y);
        let code = self.terrain.code(&self.levels, screen, x, y);
        match code {
            Some(terrain::T_WATER) | Some(terrain::T_WATER_HI) => {
                let mut clip = self.spies[i].pose_clip + 2;
                if clip >= 0x16 {
                    clip = SINK_WATER;
                }
                self.spies[i].pose_clip = clip;
                let mut t = self.spies[i].terrain_t + 1;
                if t >= 12 {
                    t = 0;
                    self.heat_step(i, -1);
                }
                self.spies[i].terrain_t = t;
            }
            Some(terrain::T_DEEP_SNOW) => {
                // `CS:1e8b` — the snowshoes **in hand** (`0x2F2C == 7`) leave the cell inert: no
                // sink, no counter, no drain — and no trudge roll either (`CS:1e92` jumps past the
                // whole arm, so the PRNG is not even called). It is the carried word, not a worn
                // flag.
                if self.spies[i].carried != 7 {
                    // **stage 23c — the trudge** (`CS:1e95`..`1e9d`). `FUN_26c0_20cd`'s low byte,
                    // **odd** -> `FUN_26c0_3396`: the spy is stopped for this pass, so `CS:0a1a`
                    // commits nothing and he covers the plate at about half speed. The sink and
                    // the counter below run either way — they sit *after* the branch.
                    //
                    // The original's roll is `((0x31B0 + second) * hundredths) & 1`, i.e. the DOS
                    // clock; the port uses its own deterministic stream (see `Game::stand_rng`),
                    // which is what keeps a seed's walk reproducible.
                    if self.stand_roll_byte() & 1 == 1 {
                        self.spies[i].stop();
                    }
                    let mut clip = self.spies[i].pose_clip + 2;
                    if clip >= 8 {
                        clip = SINK_SNOW;
                    }
                    self.spies[i].pose_clip = clip;
                    // `CS:1eb2`..`1ec6`: `incw`, `cmpw $0x8`, **`jg`** — the ninth pass steps, not
                    // the eighth. (Water next door is `jge 0xc`, the fire `jge 2`, the igloo
                    // `jle 0x14`: the snow branch is the only one whose compare is strict.)
                    let mut t = self.spies[i].terrain_t + 1;
                    if t > 8 {
                        t = 0;
                        self.heat_step(i, -1);
                    }
                    self.spies[i].terrain_t = t;
                }
            }
            Some(terrain::T_ICE) => {
                if (vx != 0 || vy != 0) && self.spies[i].anim.is_none() {
                    // `CS:1e3a`: `FUN_26c0_20cd`'s low byte must be **above 0xE6** — 25 of 256
                    let roll = self.rng.next() as u8;
                    if roll > 0xE6 {
                        // `CS:1E4E..1E5B`: the slip's falling sweep, fired before the hand is put
                        // away and before the animation starts (`315f`/`277c` follow it).
                        self.cue(sound::cue::FALL);
                        self.heat_step(i, -DAMAGE_ICE_STEPS);
                        self.stow_or_drop(i);
                        self.spies[i].act_state = round::STATE_FALL;
                        self.start_anim(i, round::STATE_FALL, None);
                        let o = 1 - i;
                        if self.spies[o].anim.is_none() {
                            self.spy_laugh(o);
                        }
                    }
                }
            }
            Some(terrain::T_FIRE) => {
                self.spies[i].stop();
                self.spies[i].terrain_t += 1;
                if self.spies[i].terrain_t >= 2 {
                    self.spies[i].terrain_t = 0;
                    self.heat_step(i, 1);
                }
            }
            Some(terrain::T_IGLOO_FLOOR_A) | Some(terrain::T_IGLOO_FLOOR) => {
                self.spies[i].terrain_t += 1;
                if self.spies[i].terrain_t > 0x14 {
                    self.spies[i].terrain_t = 0;
                    self.heat_step(i, 1);
                }
            }
            Some(0x16) => {}
            Some(terrain::T_SOLID) => {
                // `CS:1e22` — the wall veto, the same `3396` `move_once` runs
                self.spies[i].stop();
            }
            Some(0x07) | Some(0x08) => {}
            _ => {
                self.spies[i].pose_clip = 0;
            }
        }
    }

    /// `CS:1e95` — the deep-snow trudge's roll: one byte, the odd/even bit is all it uses.
    /// `stand_roll` is the test/verification hook; the round itself always takes `stand_rng`.
    fn stand_roll_byte(&mut self) -> u8 {
        match self.stand_roll {
            Some(b) => b,
            None => self.stand_rng.next() as u8,
        }
    }

    /// One game frame for one spy — everything the original's loop body does for a player.
    ///
    /// The order is the code's (`CS:0556..0a4c`):
    /// 1. the position/state gates, then the action key (`FUN_26c0_2de3`);
    /// 2. the direction keys set the velocity words — **left, right, up, down** in that order
    ///    (`CS:07cd`) — and with no key held `FUN_26c0_3396` puts the spy at rest;
    /// 3. the move, run **once or twice**. `FUN_26c0_0000` returns 1 when the viewport actually
    ///    scrolled, and the loop then jumps back and moves again (`CS:0a44..0a49`). That second
    ///    iteration is why the view scrolls 2 units a frame while the spy's own step is 1: the spy
    ///    holds his place in the window and the world goes past. At a scroll clamp the second
    ///    iteration does not happen, so the same loop walks at **half speed** there — the original's
    ///    own behaviour, and the reason the owner saw the speed change at the far edge.
    fn step_frame(&mut self, i: usize, keys: [Keys; 2]) {
        let all_keys = keys;
        let keys = keys[i];
        // `CS:71d` — a spy whose `DS:0x2F70` is set (the rocket is flying) skips his whole frame;
        // `CS:731` does the same for `DS:0x2F64 >= 2` ("out of the round", which the launch writes
        // on the *other* spy). Neither can move, act, or re-test his visitor flag.
        if self.spies[i].launched || self.spies[i].out_of_round {
            return;
        }
        // `CS:072e` — a spy whose `DS:0x2F18` ("raised") is set skips the whole key/move block and
        // runs only the animation: the hatch's descent, the bear's state and the climb back out
        // own the spy for their duration, so he cannot walk away mid-sequence. The frame's **tail**
        // still runs, though: `CS:0a3e` calls `FUN_26c0_0000` for every player, raised or not, and
        // only the frame-step hold is skipped for a raised one (`CS:09b`/`CS:130`). So a spy's own
        // view keeps following the x the *script* gives him — measured on the recorded throw,
        // `playtest4` ds_01774..01781: his x goes 116→123 while his `map_x` goes **82→94**, 2 a
        // frame, exactly one scroll a frame.
        if (self.hatch.seq != HatchSeq::Idle && self.hatch.spy == i) || self.hatch.other == Some(i) {
            // the animation branch; the scroll is run at the end of `hatch_advance`, after the
            // script itself, which is the executable's order
            self.hatch.keys[i] = keys;
            return;
        }
        // `CS:0727`: while `0x2F18` is set the whole key/act block is skipped and the spy runs only
        // his animation — the saw, the bucket, the ice pick, the slip, the fall, the knock-out and
        // the laugh all own him for their duration. `CS:097c` still calls the body-contact push and
        // the hatch touch; the port does not (named in STAGE13A_REPORT.md §5).
        if self.spies[i].anim.is_some() {
            self.spies[i].fire_prev = keys.fire;
            self.advance_anim(i);
            // **stage 24e** — `CS:0a3e`/`0xa41`: `FUN_26c0_0000` is on the **common** tail, so a
            // raised spy still scrolls his view and still re-tests the visitor flag. The animation
            // path `CS:0727` jumps to `0x97c`, which runs `35e5`, `2392`, the script's own step and
            // then falls into the same commit (`0xa1a`), the terrain probe (`0x0a3e`) and the scroll
            // (`0xa41`) as a walking spy. Skipping it made a sinking or knocked-out spy invisible to
            // the visitor rule — measured: the demo's spy 2 goes under at 466 and the recording still
            // hands *him* the visitor flag at 479 while the port handed it to spy 1.
            self.scroll_tail(i, keys);
            return;
        }
        // `CS:073b`..`075e` — the heat block, before any key is read: a frozen spy is stopped, the
        // freeze fires, and the base counter ticks. `CS:0745` runs on every path that reaches the
        // frame body's keys, which is why it sits here and not in `clock_tick`.
        self.heat_block(i);
        if self.spies[i].frozen >= 2 {
            return;
        }
        // `CS:0836`: the hazard under the feet, once per frame, before the move. The executable
        // reaches it from *every* key path (including the trapulator's and the fire press's); the
        // port resolves it here, before the keys, so a hazard preempts that frame's press instead
        // of following it — a one-frame ordering difference, 100 ms, with no recorded case that
        // separates the two (STAGE13A_REPORT.md §5).
        self.hazard_check(i);
        if self.spies[i].anim.is_some() {
            self.spies[i].fire_prev = keys.fire;
            // `CS:0979` runs before the animation branch, so even the spy who has just been put
            // into an animation feels the cell he is standing on (the ice keeps draining him).
            self.terrain_tick(i);
            // `CS:0836` falls into the move and then `CS:098c`, so the animation's first step is
            // taken in the frame the hazard fires — the recorded dumps show `0x2EFC` 0 in the same
            // dump as the new state (`iceaxe` ds_00530, `playtest2` ds_00647).
            self.advance_anim(i);
            return;
        }
        let fire_edge = keys.fire && !self.spies[i].fire_prev;
        let dir_edge = [
            keys.up && !self.spies[i].dir_prev[0],
            keys.down && !self.spies[i].dir_prev[1],
            keys.left && !self.spies[i].dir_prev[2],
            keys.right && !self.spies[i].dir_prev[3],
        ];
        let prev_dirs = [keys.up, keys.down, keys.left, keys.right];
        self.spies[i].fire_prev = keys.fire;
        // `DS:0x328F`/`DS:0x3293`: the trapulator's left/right are **latched press flags**, not key
        // levels — the handler sets one and only `FUN_26c0_2a4e` clears it, when it moves. A press
        // edge (however briefly the key was down inside the frame) sets the request; the trapulator
        // keeps it while the repeat delay counts down and clears it on the frame it moves, so a press
        // that lands inside the 3-frame repeat delay is still served instead of being dropped.
        if dir_edge[2] {
            self.spies[i].cursor_req = -1;
        } else if dir_edge[3] {
            self.spies[i].cursor_req = 1;
        }

        // `FUN_26c0_31c8` negates the kit box's blink phase once per drawn frame, open or not.
        self.spies[i].kit_blink = -self.spies[i].kit_blink;
        // `CS:2CD5` negates the **map's** word at the end of every frame it draws, which is the
        // own-position marker's blink: the routine draws that marker only while the word is
        // non-negative (`CS:2CA3`), so the map flickers at half the frame rate and nowhere else.
        if self.spies[i].map_open {
            self.spies[i].map_sign = -self.spies[i].map_sign;
        }
        // **the trapulator owns the frame while it is open** (`CS:076b`: `FUN_26c0_3396` stops the
        // spy, `FUN_26c0_2a4e` reads the keys, and the loop jumps to the frame's tail).
        if self.spies[i].trap_open {
            self.spies[i].stop();
            self.terrain_tick(i);
            self.trapulator(i, keys, fire_edge);
            // `FUN_26c0_2cda` negates the open word once per drawn frame: that is the selection's
            // flash. Flipping it here (a game frame, ten a second) is the same rate the original
            // draws at, and keeps `frame()` a pure function of the state.
            self.spies[i].trap_sign = -self.spies[i].trap_sign;
            self.spies[i].dir_prev = prev_dirs;
            return;
        }
        // button A (`CS:0782`): the fire press, once per press. The latched-tool test at
        // `CS:077b` skips it while a tool is waiting to be put away.
        if !self.spies[i].act_latch && fire_edge {
            // the stage-4 `--igloo-key` fallback still gets the action key first; in the default
            // model it is inert (the door is a walk-in cell, stage 5).
            if self.action(i, keys, fire_edge) {
                self.terrain_tick(i);
                self.spies[i].dir_prev = prev_dirs;
                return;
            }
            self.terrain_tick(i);
            self.fire_tap(i);
            // **stage 25a** — `CS:78e` is a `jmp 0x836`, and `0x836` flows on through the door
            // passes to `CS:976`/`0x979` (the terrain) and then `CS:97c` → `CS:982`, which is the
            // animation's own step. So the frame an item is taken **steps the new state once**:
            // `FUN_26c0_277c` seeds `0x2EFC = -2` inside the take and the same frame's `CS:98c`
            // makes it 0, which is the word the recording's take dump holds (`demo-full-24c`
            // ds_00572: `0x2F18` = 1, `0x2EFC` = 0, `0x2EF8` = 132 — the state-0 pose).
            //
            // That is what makes the raise cost exactly **two** frame bodies (two skipped heat
            // ticks: `CS:0727` sends a raised spy past the heat block), which is what the base
            // counter shows at both demo takes — 11 → 18 over the nine frames 12..21, and 45 → 50
            // over 688..695.
            self.advance_anim(i);
            // **the direction edge survives this frame.** With one fire key the press and the
            // direction that turns it into "button B" usually arrive in the same frame; consuming
            // the edge here would make burying impossible unless the key order was lucky. The
            // original has two buttons, so the question cannot arise there (STAGE7_REPORT.md 2.3).
            return;
        }
        // button B (`CS:0791`): the second button together with a direction — the tool's own
        // action, or burying a part / the case. It is tested on *every* frame, not only while the
        // put-away latch is set: that is where `FUN_26c0_2d7e` sits in the loop. Two ways in,
        // because the port has one key and the machine has two: the computer sets B itself
        // (`keys.fire_b`, a level), the player reaches B as fire-held-plus-a-direction-edge.
        //
        // **stage 18f** — the original has two buttons and this is the second one's flag
        // (`DS:0x3283`, tested as a *level*): the computer sets it on every frame of the punch
        // (`CS:1a46`), so its throw repeats. The port's `fire` is button A, whose flag is consumed
        // by acting, so it must not stand in for B on a computer spy — that collapse is what made
        // the AI bury and re-dig its own briefcase for 2,373 frames (STAGE18F_REPORT.md §3).
        let b_level = [keys.up, keys.down, keys.left, keys.right];
        let any = |d: [bool; 4]| d[0] || d[1] || d[2] || d[3];
        let b_dir = if keys.fire_b {
            // the second button's own test: `0x2d7e` reads the direction *flag* (`(%di) == 1`),
            // which the input scan re-sets on every frame it is held — so this is a level, not an
            // edge. Pulsing `dir_prev` instead would also force the trapulator's cursor request
            // every frame, which the panel's own 3-frame repeat then has to absorb.
            if any(b_level) { Some(b_level) } else { None }
        } else if keys.fire && any(dir_edge) {
            Some(dir_edge)
        } else {
            None
        };
        if let Some(bd) = b_dir {
            self.terrain_tick(i);
            self.directed_action(i, bd);
            self.spies[i].act_latch = false;
            self.spies[i].dir_prev = prev_dirs;
            // `CS:079d` jumps to the hazard check and then the frame's tail, so the animation's own
            // first frame runs in the frame the action starts — which is what makes the seeded
            // `0x2EFC = -2` never a drawn value (`CS:09a2`..`0a2e`).
            self.hazard_check(i);
            self.advance_anim(i);
            // **stage 24g** — `CS:079d` is a `jmp 0x836`, and `0x836` is the *pre-move* entry: the
            // positional commit (`0xa1a`), the terrain probe (`0xa3e`) and `FUN_26c0_0000`
            // (`0xa41`) all run for a spy on the second button too. The port returned here, so a
            // spy holding B never re-tested the visitor flag — measured: the demo's spy 2 holds
            // right+B for frames 709..732 and the recording drops his `0x2F80` at 721 the moment
            // spy 1's view carries him out of the window (`CS:1c3`: `map(host) - 4 >= x`), while
            // the port kept the flag and his view at 108 until the button went up.
            self.scroll_tail(i, keys);
            return;
        }
        // `CS:07a0`: releasing fire is what the button-A *tap* is, with one key — the thing in hand
        // goes back into the trapulator (`FUN_26c0_315f`) or on the ground.
        // **port decision, not the original's timing**: the original puts it away on the very next
        // frame (one 100 ms frame later), which leaves no time to add a direction and so no way to
        // bury with a single fire key. See STAGE7_REPORT.md §2.3.
        if self.spies[i].act_latch && !keys.fire {
            self.terrain_tick(i);
            self.stow_or_drop(i);
            self.spies[i].dir_prev = prev_dirs;
            return;
        }
        self.spies[i].dir_prev = prev_dirs;

        // **stage 18c — `CS:07cd`: while the map is up the direction keys are ignored.** The gate
        // sits above the latch clear and the whole move, so the frame's walk is skipped; it sits
        // *below* `CS:07c2`'s "no direction key at all → come to rest", so a spy with an empty
        // keyboard still stops, and both heat and terrain tick above it either way.
        if self.spies[i].map_open {
            if !(keys.up || keys.down || keys.left || keys.right) {
                self.spies[i].stop();
            }
            self.terrain_tick(i);
            return;
        }

        // `CS:07cd`: every path that holds a direction key clears the two button-A latches —
        // `0x2F34` (the first half of the double tap that opens the trapulator) and `0x2F28` (the
        // put-away latch). So two taps with a step in between do **not** open the panel, and a
        // direction press abandons a pending put-away. Both were missing before stage 8.
        if keys.up || keys.down || keys.left || keys.right {
            self.spies[i].fire_taps = 0;
            self.spies[i].act_latch = false;
        }

        if self.action(i, keys, fire_edge) {
            self.terrain_tick(i);
            return;
        }

        if keys.left {
            self.set_left(i);
        } else if keys.right {
            self.set_right(i);
        } else if keys.up {
            self.set_up(i);
        } else if keys.down {
            self.set_down(i);
        } else {
            self.spies[i].stop();
            // `CS:07c7` stops him and falls into the hazard check and the terrain dispatch, which
            // is why a standing spy still feels the cell he is on (`CS:0944` with a zero velocity)
            self.terrain_tick(i);
            // `FUN_26c0_0000` is on the common path of the per-player loop, not part of the
            // movement branch: a spy who is holding nothing still runs it, so his visitor flag is
            // re-tested every frame. Skipping this (stage 4) is why a host walking away left the
            // other console blank until the visitor himself moved — defect C.
            // `--visitor-eval-mover-only` restores that stage-4 behaviour.
            if !self.visitor_eval_mover_only {
                self.update_visitor(i);
            }
            return;
        }

        // `CS:0944..0979` — the cell this frame's own velocity is stepping onto, probed before the
        // commit two instructions later (`CS:0a1a`) — is dispatched from **inside** the loop below:
        // each pass of the body probes once (`move_once`), so a frame that covers two units ticks
        // the terrain counter twice (stage 22a).
        // The loop's own re-entry (`CS:0a41..0a49`): `FUN_26c0_0000` returns 1 when the viewport
        // actually scrolled, and the frame then **jumps back to `CS:07cd`** and moves again — as
        // many times as it keeps scrolling, not once. In the steady state that is two iterations
        // (one scroll, then one step inside the dead zone), which is why a walking spy covers 2
        // units a frame while the view scrolls and 1 while it does not. A spy who is *past* the
        // dead zone (a door step, a teleport) catches up over several iterations in a single frame;
        // the recordings show it (26 of 2,604 map-changing dump pairs carry 4 units of `map_x`).
        // Stage 9b capped this at two iterations, so the port could never do more than 4.
        let mut scrolled = false;
        for _ in 0..16 {
            if !self.move_once(i, keys) {
                break;
            }
            scrolled = true;
        }
        self.update_visitor(i);
        // `CS:0743` — the tail of the move loop: on a frame that scrolled, the original drags the
        // other spy along. It runs after `FUN_26c0_0000`'s visitor tail, so the flags it tests are
        // this frame's. See [`Game::push_in_step`].
        if scrolled {
            self.push_in_step(i, all_keys);
        }
    }

    /// `CS:0743..07b1` — **the walk in step**, the tail of the move loop.
    ///
    /// On a frame where this spy's own move actually **scrolled** the view (`DS:0x31A6`, set inside
    /// `FUN_26c0_0000` and cleared at the top of every player's frame body), the original drags the
    /// *other* spy one unit in the same direction — provided they are on one shared screen and the
    /// other spy is walking the same way. It is how two spies on one console keep their spacing while
    /// one of them pushes the edge, and it is the rule that made the recorded pairs keep a constant
    /// separation over every run of the `orig-compare-1` session.
    ///
    /// ```text
    /// 0743  if (this.0x31A6 == 0 || 0x2F7E + 0x2F80 == 0) skip
    /// 074e  if (other.0x3283 == 1) skip                 ; the other is on the action key
    /// 0758  if (other.0x2F18 != 0 || other.0x2F64 != 0) skip   ; raised, or out of the round
    /// 0764  if (right(this) == 1 && right(other) == 1) {
    /// 0770      other.x += 1
    /// 0774      FUN_26c0_366e(); FUN_26c0_1e0f()        ; the terrain probe at the new cell
    /// 0786      if (0x31AA == 1) other.x -= 1           ; refused -> undo
    ///       }
    /// 07a2  else if (left(this) == 1 && left(other) == 1) { the same, -1 }
    /// ```
    ///
    /// Only the **horizontal** keys are pushed: two spies walking up or down a shared screen just
    /// each take their own unit, which is why the recorded pairs show `dy 1/1` with no `x` movement.
    ///
    /// What the push deliberately does **not** do: it does not run the body collision (`0x35e5`),
    /// the hatch trigger (`0x2392`) or the frame step — the code writes `other.x` and nothing else,
    /// so the shoved spy slides one unit without advancing his walk cycle, and cannot be shoved
    /// onto a hatch or through the pusher. The probe is the port's own terrain veto
    /// ([`Game::terrain_solid`]), i.e. the same cells a walk refuses; `FUN_26c0_1e0f`'s door and
    /// igloo sub-branches are not run for a shoved spy (named open item: no recording shows a shove
    /// into a doorway).
    fn push_in_step(&mut self, i: usize, keys: [Keys; 2]) {
        if self.players < 2 {
            return;
        }
        let o = 1 - i;
        // `CS:0748` — 0x2F7E + 0x2F80: somebody is on the other's board.
        if !self.spies[0].blank && !self.spies[1].blank {
            return;
        }
        // `CS:074e` — the other spy's **second** button (`0x3283`, `keys.fire_b`) must be up. The
        // word is the B latch, not A: a spy holding B skips his whole move block at `CS:0791`
        // (`FUN_26c0_2d7e` -> `0x836`), so he is not walking even with a direction down, and the
        // shove must not drag him. Testing `0x327F` (A) instead let a held-B spy be pushed one unit
        // a frame by his partner's scroll — the demo's spy 2 rides from x 76 to 90 over frames
        // 717-730 while the recording holds him at 76, which moves him out of the visitor window
        // and keeps the flag (`0x2F80`) and his view (`0x24A8`) at 108 five frames too long.
        if keys[o].fire_b {
            return;
        }
        // `CS:0758` — and he must be a free walker: not raised, not out of the round.
        if self.spies[o].pose_raised || self.spies[o].out_of_round {
            return;
        }
        let step = if keys[i].right && keys[o].right {
            1
        } else if keys[i].left && keys[o].left {
            -1
        } else {
            return;
        };
        let x = self.spies[o].x();
        let cand = x + step;
        if cand < terrain::X_MIN || cand > terrain::X_MAX {
            return;
        }
        // **stage 23c — the shove is a move *and* a dispatch** (`CS:0A97..0x0AC4`, the left arm is
        // the mirror at `0x0AC7..0x0AFD`):
        //
        // ```text
        // 0a9b  decw 0x329b(%si)            ; shove him one unit (the left arm: +1 at 0x0AD4)
        // 0aab  word[0x31AA] = 0            ; clear "refused"
        // 0ab1  call 366e                   ; probe the cell he was shoved onto
        // 0ab4  call 1e0f                   ; HIS TERRAIN DISPATCH, a second time this frame
        // 0ab9  if (0x31AA == 1) incw 0x329b(%bx)   ; the dispatch stopped him -> undo the shove
        // ```
        //
        // Three ordering rules, each from an address:
        //  1. the move happens **before** the dispatch, so the cell it reads is the shoved one;
        //  2. `0x31AA` is cleared **before** it and only by this routine, so a stop from the
        //     victim's own earlier pass in the same frame does not leak in;
        //  3. the undo restores the **x only** — the sink, the cold counter and any animation the
        //     dispatch started are kept, which is what `0x0AB9` literally does.
        self.spies[o].x3 = cand * SUB;
        self.spies[o].refused = false;
        self.terrain_tick(o);
        if self.spies[o].refused {
            self.spies[o].x3 = x * SUB;
        }
    }

    // ---- the direction setters (FUN_26c0_32b4 / 32ed / 3326 / 335e) ------------------------
    //
    // Each writes one velocity vector, the direction word, the mirror flag and the frame ceiling,
    // and fixes up the frame step. The horizontal pair leaves the frame where it was (a spy who
    // starts walking again carries on through the cycle); the vertical pair, if the spy was
    // standing (step 0), restarts the cycle at frame 0.

    fn set_left(&mut self, i: usize) {
        let s = &mut self.spies[i];
        s.vel_x = -WALK_X_PER_FRAME;
        s.vel_y = 0;
        s.dir = 0;
        s.mirror = true;
        if s.frame_step != ANIM_STEP && s.frame_step != -ANIM_STEP {
            s.frame_step = ANIM_STEP;
        }
    }

    fn set_right(&mut self, i: usize) {
        let s = &mut self.spies[i];
        s.vel_x = WALK_X_PER_FRAME;
        s.vel_y = 0;
        s.dir = 0;
        s.mirror = false;
        if s.frame_step != ANIM_STEP && s.frame_step != -ANIM_STEP {
            s.frame_step = ANIM_STEP;
        }
    }

    fn set_up(&mut self, i: usize) {
        let s = &mut self.spies[i];
        s.vel_x = 0;
        s.vel_y = -WALK_Y_PER_FRAME;
        s.dir = 2;
        s.mirror = false;
        if s.frame_step == 0 {
            s.frame = 0;
            s.frame_step = ANIM_STEP;
        }
    }

    fn set_down(&mut self, i: usize) {
        let s = &mut self.spies[i];
        s.vel_x = 0;
        s.vel_y = WALK_Y_PER_FRAME;
        s.dir = 4;
        s.mirror = false;
        if s.frame_step == 0 {
            s.frame = 0;
            s.frame_step = ANIM_STEP;
        }
    }

    /// One iteration of the move: the candidate position, the door threshold, the world-row x
    /// bounds, the terrain veto (`FUN_26c0_366e` + `FUN_26c0_1e0f`), the commit and the scroll.
    /// Returns `true` when the scroll moved, which is when the caller runs a second iteration.
    fn move_once(&mut self, i: usize, keys: Keys) -> bool {
        let (vx, vy, x, y, screen) = {
            let s = &self.spies[i];
            (s.vel_x, s.vel_y, s.x(), s.y(), s.screen)
        };

        // --- a vertical step past the band is a door (`CS:0845`/`08a3`) -------------------
        // The door arms write the screen and the entry row (`0x898`/`0x8e7`: `y = 0x32BF` /
        // `0x32BB`) and then **fall through** to the same probe, body test and commit as any other
        // step (`0x96a`..`0xa1a`) — the crossing pass is a normal pass with a new screen.
        //
        // **Stage 24e** — two things the port used to get wrong here, both measured on the demo's
        // spy 2:
        //  * it **returned** after the door, so every crossing lost the pass's velocity step and the
        //    spy came out one step short (room 4: y 52 against the recording's 54, frames 206-240,
        //    where he then stood on his friend's row and both were blocked — the frames 610-620
        //    stall that put the rest of the demo out of step);
        //  * it probed the cell the spy was about to stand on. `CS:0976` hands `FUN_26c0_366e` the
        //    **candidate** words, and a door arm has just rewritten `0x3297` — but not `0x32C3`. So
        //    the crossing pass probes the *old* row (32 or 60) in the **new** mask: the up-door arm
        //    reads row 0 (the new room's top edge), which is solid off the door column and stops him
        //    on the entry row (`y` stays 58 — the recording's frames 48-49); the down-door arm reads
        //    row 13, the byte *past* the mask (`0x36b4`'s `mul` has no bound), which is open, so his
        //    velocity survives and he lands on `Y_MIN + 2` (`36` — the recording's frame 202).
        let (vx, vy, x, y, screen, probe) = if vy != 0 && (y + vy < Y_MIN || y + vy > Y_MAX) {
            let probe_y = y + vy;
            if !self.door_step(i, vy < 0) {
                return false;
            }
            let s = &self.spies[i];
            (s.vel_x, s.vel_y, s.x(), s.y(), s.screen, Some(probe_y))
        } else {
            (vx, vy, x, y, screen, None)
        };

        // --- the world row's x bounds: DS:0x32B3/0x32B7 = 0 .. 304 ------------------------
        let mut cand_x = x + vx;
        if cand_x < terrain::X_MIN || cand_x > terrain::X_MAX {
            self.spies[i].vel_x = 0;
            cand_x = x;
        }
        let cand_y = probe.unwrap_or(y + vy);

        // --- `CS:0944..0979`: the probe and `FUN_26c0_1e0f` — **once per pass** --------------
        // The cell is `(x + 0x2F04, y + 0x2F00)` *after* the clamp above (`CS:0950..0962` zeroes
        // the velocity and re-reads the standing x`). A crossing pass probes at the candidate row it
        // computed **before** the door arm rewrote `0x3297` (`CS:0976`).
        if probe.is_some() {
            self.terrain_tick_at(i, screen, cand_x, cand_y);
        } else {
            self.terrain_tick(i);
        }

        if vx == 0 && vy == 0 {
            return false;
        }
        // `CS:0982`: a dispatch that started an animation (`1e0f`'s ice slip calls `277c`) ends the
        // pass here — the commit below would add the velocity the animation just zeroed.
        if self.spies[i].anim.is_some() {
            return false;
        }

        // --- the switch's **vetoes** (`FUN_26c0_1e0f`'s `3396` arms) ----------------------
        //
        // The dispatch itself has already run, above; what is left here is what the caller does
        // with a cell it cannot enter. The solid codes go through `FUN_26c0_3396`, which zeroes
        // both velocities —
        // so the commit two instructions later writes the unchanged position: the spy stops dead,
        // on the standing frame. The igloo codes *move* the spy inside `FUN_26c0_1e0f` itself and
        // then let the caller's commit run, which is why a walk-in entry lands one unit short of
        // the `+0xF` it writes.
        // **stage 23c** — `CS:0a1a` commits the velocity word **as the dispatch left it**. A
        // dispatch that ran `FUN_26c0_3396` — the wall, the fire, and now the deep-snow trudge —
        // wrote zero there, so this pass adds nothing and the spy stays on the cell he is on.
        // `cand_x`/`cand_y` above stay the **probed** cell: the probe is taken before the dispatch
        // can stop him, which is why the terrain is still read at one unit ahead.
        let mut commit_x = x + self.spies[i].vel_x;
        let mut commit_y = y + self.spies[i].vel_y;
        match self.terrain.code(&self.levels, screen, cand_x, cand_y) {
            Some(terrain::T_SOLID) | Some(terrain::T_SOLID_SNOW) => {
                self.spies[i].stop();
                return false;
            }
            Some(terrain::T_CLIMB) | Some(terrain::T_CLIMB_HI) if self.void_solid => {
                self.spies[i].stop();
                return false;
            }
            Some(terrain::T_IGLOO_IN)
                if self.igloo_entry == IglooEntry::Walk
                    && self.level_at(screen + 10, self.terrain.col_of(x)).is_some() =>
            {
                // `AX == 7`: the other spy standing inside *this* igloo blocks the door
                // (`if (other.screen > 9 && other.screen - 10 == this.screen) { stop; return; }`).
                if self.igloo_occupied(i) {
                    self.spies[i].stop();
                    return false;
                }
                let base = self.terrain.gate(self.terrain.col_of(x));
                self.igloo_enter(i, base);
                // the caller's commit still adds this frame's velocity (`CS:0a1a`)
                commit_x = self.spies[i].x() + vx;
                commit_y = self.spies[i].y() + vy;
            }
            Some(terrain::T_IGLOO_OUT) if screen >= FIRST_INTERIOR_SCREEN =>
            {
                // `AX == 8`: back out to `screen - 10` at the exterior door line. The only other
                // code-8 cells in the tables are in the *exterior* igloo mask (level 16, x-offset
                // 18, y 40/42), behind nineteen solid units of body — no spy can reach them, so
                // they stay inert rather than firing a nonsensical `screen - 10`.
                let base = self.terrain.gate(self.terrain.col_of(x));
                self.igloo_exit(i, base);
                commit_x = self.spies[i].x() + vx;
                commit_y = self.spies[i].y() + vy;
            }
            _ => {}
        }

        // --- the other spy's body (`CS:097c` → `FUN_26c0_35e5`) ---------------------------------
        //
        // The terrain's veto is followed, two instructions later, by the **only** spy-vs-spy test in
        // the executable. It is a plain blocking collision — it calls the same `FUN_26c0_3396` the
        // walls do, so the spy stops on the standing frame and nothing else happens: no state is
        // written to either spy (no stun, no `0x0C`, no knock-out), nothing leaves either hand and
        // no score or heat word is touched. `STAGE9B_REPORT.md` §1 has the whole routine and §2 the
        // measurements that agree with it.
        // 35e5's first instruction — the blocked flag is cleared at the top of every step, so it
        // says "my *last* step was refused by the other spy's body", exactly like the original's
        // `DS:0x2FA4`.
        self.spies[i].blocked = false;
        if self.spy_block && self.spy_contact(i, cand_x, cand_y) {
            self.spies[i].stop();
            self.spies[i].blocked = true;
            return false;
        }

        // --- commit (`CS:0a1a`) ----------------------------------------------------------
        {
            let s = &mut self.spies[i];
            s.x3 = commit_x * SUB;
            s.y3 = commit_y * SUB;
            s.advance_frame();
        }

        // --- the scroll (`FUN_26c0_0000`) ------------------------------------------------
        self.scroll(i, keys)
    }

    /// Is the terrain at this spot one the original refuses? `void_solid` additionally makes the
    /// climb code solid — a flag, off by default, because the original walks on it.
    fn terrain_solid(&self, screen: u16, x: i32, y: i32) -> bool {
        match self.terrain.code(&self.levels, screen, x, y) {
            Some(terrain::T_SOLID) | Some(terrain::T_SOLID_SNOW) => true,
            Some(terrain::T_CLIMB) | Some(terrain::T_CLIMB_HI) => self.void_solid,
            _ => false,
        }
    }

    /// Is the other spy standing inside the igloo this spy is about to walk into? The original's
    /// guard is `if (other.screen > 9 && other.screen - 10 == this.screen) { FUN_26c0_3396(); return; }`
    /// — the door refuses while the igloo is occupied, and the spy stays where he is.
    fn igloo_occupied(&self, i: usize) -> bool {
        if self.players < 2 {
            return false;
        }
        let o = 1 - i;
        let inside = self.spies[o].screen;
        inside >= FIRST_INTERIOR_SCREEN && inside - FIRST_INTERIOR_SCREEN == self.spies[i].screen
    }

    /// **The two spies' bodies block each other** — `FUN_26c0_35e5`, called from the per-player
    /// frame loop at `CS:097c`, between the terrain switch (`CS:0979`) and the hatch touch
    /// (`CS:097f`). The whole routine, in its own order:
    ///
    /// ```text
    /// 35e5  this.0x2FA4 = 0                                  ; the blocked flag, cleared every step
    /// 35eb  bp = si ^ 2                                      ; the OTHER spy
    /// 35f3  if (other.screen != this.screen) return
    /// 3601  if (other.0x2F64 == 2) return                    ; he is out of the round
    /// 360c  if (0x2F7E + 0x2F80 == 0) return                 ; nobody is a visitor: not one board
    /// 3618  if (this.0x2F18 != 0) return                     ; I am mid-animation
    /// 361f  ax = other.y + 0x2F1C(other); if (ax < this.tent_y) return
    /// 362f  ax = other.y - 0x2F1C(other); if (ax > this.tent_y) return
    /// 363f  ax = other.x - 0x2F20(this) + 3; if (ax > this.tent_x) return
    /// 3651  ax = other.x + 0x2F20(other) - 3; if (ax < this.tent_x) return
    /// 3664  FUN_26c0_3396                                     ; zero the velocities = refuse the step
    /// 3667  this.0x2FA4 = 1
    /// ```
    ///
    /// So: same room, at least one spy on the other's board (the visitor flag), the mover not
    /// mid-animation, `|Δy| <= 1` and `|Δx| <= 3` **against the tentative position** (the word the
    /// mask test at `CS:096a` just used). The x test is asymmetric in the code — the left bound uses
    /// *my* `0x2F20` and the right bound the *other's* — which is unobservable because the data
    /// image holds 6 for both.
    ///
    /// The caller must have set [`SpyState::blocked`] from the return value; this only reads.
    fn spy_contact(&self, i: usize, cand_x: i32, cand_y: i32) -> bool {
        if self.players < 2 {
            return false;
        }
        let (me, other) = (&self.spies[i], &self.spies[1 - i]);
        // 35f3 — the test is per *board*: two spies in different rooms can never touch.
        if other.screen != me.screen {
            return false;
        }
        // 360c — `0x2F7E + 0x2F80 == 0`: on one screen but with nobody on the other's board, each
        // spy is alone in his own console and there is nothing to bump into. (Measured: both flags
        // are clear in 2,522 of the 4,650 recorded same-screen dumps, and in the 211 of those that
        // are also on one row the two are never closer than **12** units — a whole console apart.
        // So the guard is consistent with the corpus even though no recording can separate it from
        // the box test.)
        if !me.blank && !other.blank {
            return false;
        }
        // 3618 — `this.0x2F18`: a spy inside a scripted animation (the hatch descent, the bear, the
        // climb out) does not run the movement block at all, so he can neither be blocked nor block.
        if me.pose_raised {
            return false;
        }
        // 361f/362f — the vertical test uses only the *other* spy's `0x2F1C`, and compares it with
        // my whole (integer) tentative row: |Δy| <= SPY_CONTACT_Y.
        if !(other.y() - SPY_CONTACT_Y..=other.y() + SPY_CONTACT_Y).contains(&cand_y) {
            return false;
        }
        // 363f/3651 — the horizontal window, narrowed by the `+3`/`-3` inset.
        let lo = other.x() - SPY_CONTACT_X + SPY_CONTACT_INSET;
        let hi = other.x() + SPY_CONTACT_X - SPY_CONTACT_INSET;
        (lo..=hi).contains(&cand_x)
    }

    /// The `AX == 7` branch of `FUN_26c0_1e0f`, in its own order:
    ///
    /// ```text
    /// screen += 10
    /// this.flag = 0
    /// if (other.flag != 0 && other.state != 2) { other.map_x = this.map_x; other.flag = 0 }
    /// this.map_x = colbase - 8
    /// this.x     = colbase + 0xF
    /// this.y     = 0x30
    /// ```
    ///
    /// The caller's commit then adds the frame's velocity, so a spy walking west lands on
    /// `colbase + 14` — which is what all 36 recorded entries show.
    fn igloo_enter(&mut self, i: usize, base: i32) {
        let o = 1 - i;
        let old_map = self.spies[i].map_x;
        {
            let s = &mut self.spies[i];
            s.screen += 10;
            s.blank = false;
            s.map_x = (base + IGLOO_ENTRY_MAP_OFF).clamp(0, MAP_X_MAX);
            s.x3 = (base + IGLOO_ENTRY_X_OFF) * SUB;
            s.y3 = IGLOO_ENTRY_Y * SUB;
            s.frame = STAND_FRAME;
            s.frame_step = 0;
        }
        if self.players > 1 && self.spies[o].blank {
            // the other spy was watching this room; he gets the entering spy's old view and his
            // own console back
            self.spies[o].blank = false;
            self.spies[o].map_x = old_map;
        }
    }

    /// The `AX == 8` branch of `FUN_26c0_1e0f`, in its own order:
    ///
    /// ```text
    /// x = colbase + 0x16;  y = 0x2A;  screen -= 10
    /// if (other.screen == this.screen && other.state != 2
    ///     && other.map_x - 4 < this.x && this.x < other.map_x + 0x29) this.flag = 1
    /// ```
    ///
    /// `map_x` is **not** touched: the caller's commit adds the frame's velocity and the scroll then
    /// runs from the interior's stale view, which is why the recorded exits land on `colbase + 24`
    /// (two iterations: the first commit scrolls the view to `colbase - 6`) far more often than on
    /// `colbase + 23` (one).
    fn igloo_exit(&mut self, i: usize, base: i32) {
        let window = {
            let s = &mut self.spies[i];
            s.screen -= 10;
            s.x3 = (base + 22) * SUB;
            s.y3 = 42 * SUB;
            s.frame = STAND_FRAME;
            s.frame_step = 0;
            (s.screen, s.x())
        };
        if self.players < 2 {
            return;
        }
        let o = 1 - i;
        if self.spies[o].screen != window.0 {
            return;
        }
        let host = self.spies[o].map_x;
        if self.spies[o].blank {
            // the original tests only the screen, the other spy's state and the window; the port
            // adds "and he is not himself a visitor", because two flags standing at once is the one
            // state the rest of the model (and every recorded row) excludes
            return;
        }
        if window.1 > host + VISITOR_ENTRY_LEFT && window.1 < host + VISITOR_ENTRY_RIGHT {
            self.spies[i].blank = true;
        }
    }

    /// The door step: the spy's `y` left the band. The level word at his column decides whether
    /// there is a door there (2/24 upward, 10 downward) and the x window is the mask's own gap
    /// (`DOOR_CX_MIN..=DOOR_CX_MAX`). The scroll is **not** reset — the recorded dumps keep it and
    /// let the dead zone pull it back over the next frames.
    fn door_step(&mut self, i: usize, up: bool) -> bool {
        let (x, screen) = {
            let s = &self.spies[i];
            (s.x(), s.screen)
        };
        let col = self.terrain.col_of(x);
        let cx = x - self.terrain.gate(col);
        let in_window = (DOOR_CX_MIN..=DOOR_CX_MAX).contains(&cx);
        let level = self.level_at(screen, col);
        let ok = in_window
            && if up {
                level.map(|lv| LEVEL_DOOR_UP.contains(&lv)).unwrap_or(false)
            } else {
                level == Some(LEVEL_DOOR_DOWN) && screen >= 2
            };
        if !ok {
            self.spies[i].vel_y = 0;
            return false;
        }
        {
            let s = &mut self.spies[i];
            s.screen = if up { screen + 2 } else { screen - 2 };
            s.y3 = (if up { DOOR_ENTRY_FROM_BELOW_Y } else { DOOR_ENTRY_FROM_ABOVE_Y }) * SUB;
        }
        self.after_room_step(i);
        true
    }

    /// After a room step: drop the visitor flag and re-test with the room-entry window
    /// (`CS:0854..091f`).
    ///
    /// The order is the code's, and it matters — this is the shared-screen door-exit defect:
    ///
    /// ```text
    /// 0859  if (v1 + v2 == 0) goto 0898            ; nobody was a visitor, nothing to drop
    /// 0862  bx = other
    /// 0864  if 0x2F7E(this) != 0 goto 0887         ; **the leaver was the visitor**
    /// 086f      if 0x2F64(other) == 2 goto 0898
    /// 0876      v(other) = 0;  map(other) = map(this)
    /// 0887  v(this) = 0;  map(this) = x(this) - 0x16
    /// 0898  y(this) = the band bound he came in through
    /// 08ef  bx = other
    /// 08f5  if 0x2F64(other) == 2 goto 0944
    /// 08fc  if screen(other) != screen(this) goto 0944
    /// 0906  if map(other) > x(this) goto 0944
    /// 0910  if map(other) + 0x2C < x(this) goto 0944
    /// 0919      v(this) = 1;  map(this) = map(other) ±/- 0x2C
    /// ```
    ///
    /// Two branches, not one: a leaver who was the **visitor** clears *his own* flag and takes his
    /// scroll from his **own** x (`x - 0x16`), and the other spy is left completely alone; only a
    /// leaver who was **not** a visitor hands his scroll to a visitor he is leaving behind. The port
    /// used to run the second branch unconditionally, so a visitor who walked out through a door
    /// un-blanked with the *host's* scroll — the owner's "his screen lights up showing the wrong
    /// scene and the spy is not visible": the new room was composed around a column he was not
    /// standing at, often with him off the 44-unit window entirely.
    fn after_room_step(&mut self, i: usize) {
        let was_visitor = self.spies[i].blank;
        self.spies[i].blank = false;
        if self.players < 2 {
            return;
        }
        let o = 1 - i;
        if was_visitor {
            // `CS:0887`: the leaver drops his own flag and keeps a view of his own position.
            let own = self.spies[i].x() - VISITOR_OWN_MAP_OFFSET;
            self.spies[i].map_x = own.clamp(0, MAP_X_MAX);
        } else if self.spies[o].blank {
            // `CS:0876`: the one left behind stops being a visitor and follows the leaver's scroll.
            self.spies[o].blank = false;
            self.spies[o].map_x = self.spies[i].map_x;
        }
        if self.spies[i].screen == self.spies[o].screen {
            let host = self.spies[o].map_x;
            let x = self.spies[i].x();
            if x >= host && x <= host + VISITOR_STEP_RIGHT {
                self.spies[i].blank = true;
                self.spies[i].map_x = if self.spies[i].x() < self.spies[o].x() {
                    (host - VISITOR_STEP_RIGHT).clamp(0, MAP_X_MAX)
                } else {
                    (host + VISITOR_STEP_RIGHT).clamp(0, MAP_X_MAX)
                };
            }
        }
    }

    /// `FUN_26c0_0000` — the scroll. `true` means the view moved, and the caller runs the move a
    /// second time in the same frame.
    ///
    /// * the **owner** of the moving scroll is the host: a visitor's movement scrolls the host's
    ///   view (`SI^2` in the code when `0x2F7E` is set);
    /// * the dead zone is 6 units on the left, 30 on the right, and the scroll only happens while
    ///   the matching key is held;
    /// * at a clamp the view does not move, so no second iteration happens — this is the original's
    ///   half-speed at the ends of the row.
    fn scroll(&mut self, i: usize, keys: Keys) -> bool {
        let visitor = self.spies[i].blank && self.players > 1;
        let owner = if visitor { 1 - i } else { i };
        let (x, map) = (self.spies[i].x(), self.spies[owner].map_x);
        let delta = if x - SCROLL_LEFT_EDGE < map {
            if !keys.left {
                return false;
            }
            -SCROLL_STEP
        } else if keys.right {
            if x <= map + SCROLL_RIGHT_EDGE {
                return false;
            }
            SCROLL_STEP
        } else {
            return false;
        };

        if visitor {
            // The visitor branch's own guards (`CS:00b8..011d`), read one by one. The scroll owner is
            // the **host** (`bx = SI^2` on the visitor path), so the host's view is what a visitor's
            // own movement drags — but only under the conditions the assembly tests:
            //
            //   positive delta (`CS:00c6..00df`): `if map+0x2C-0x0A (0x22) <= x` → **no** scroll
            //   then       (`CS:00e2..00f1`): `if host.x-8 < host.map`        → no scroll
            //   negative delta (`CS:00f4..010a`): `if x-4 < map`              → **no** scroll
            //   then       (`CS:010d..011d`): `if host.map+0x2C-0x10 (0x1C) < host.x` → no scroll
            //
            // Stage 8b fixed the two *middle* tests (they were inverted, which let a visitor past
            // the host's right edge drag the view while the host's own frame scrolled it too — 4
            // units of `map_x` a frame). Stage 10 fixes the **fourth**: stage 8b wrote it as
            // `SCROLL_RIGHT_EDGE - 0x10` (= 14), reading the addend as the dead zone's 0x1E; the
            // code adds 0x2C (the window's width) and subtracts 0x10, which is 0x1C (28). With 14
            // a west push stopped as soon as the host reached screen x 14 — the owner's "he stops
            // being able to push when the other player has ended up at the half-way mark" — while
            // an east push ran to the host's own edge. The recordings land on 6 (east) and 30
            // (west): `VISITOR_PUSH_EAST_MIN` / `VISITOR_PUSH_WEST_MAX`.
            let (ox, omap) = (self.spies[owner].x(), self.spies[owner].map_x);
            if delta > 0 {
                if x >= map + VISITOR_PUSH_SELF_EAST {
                    return false;
                }
                if ox - VISITOR_PUSH_EAST_MIN < omap {
                    return false;
                }
            } else {
                if x - VISITOR_PUSH_SELF_WEST < map {
                    return false;
                }
                if omap + VISITOR_PUSH_WEST_MAX < ox {
                    return false;
                }
            }
        }

        let next = map + delta;
        if next < 0 {
            self.spies[owner].map_x = 0;
            return false;
        }
        if next > MAP_X_MAX {
            self.spies[owner].map_x = MAP_X_MAX;
            return false;
        }
        self.spies[owner].map_x = next;
        // the view moved under the spy: the frame step already taken this iteration is undone, and
        // the second iteration takes it again — one animation step per frame, two units of travel.
        if !self.spies[i].pose_raised {
            self.spies[i].frame = (self.spies[i].frame as i32 - self.spies[i].frame_step as i32)
                .clamp(0, ANIM_MAX as i32) as u16;
        }
        true
    }

    /// **The frame's tail for a spy who is not walking** — `CS:0a41`: `FUN_26c0_0000` (the scroll,
    /// and the visitor re-test at its `0x174..0x202`), which the original runs for every player that
    /// is neither launched nor out of the round. A raised spy reaches it from `0x97c` (after `35e5`
    /// and `2392`) and re-enters it when the view scrolled (`0xa49` jumps back to `0x7cd`, which the
    /// raised path skips through `0x727` and so runs only the tail again).
    fn scroll_tail(&mut self, i: usize, keys: Keys) {
        for _ in 0..16 {
            if !self.scroll(i, keys) {
                break;
            }
        }
        self.update_visitor(i);
    }

    /// `FUN_26c0_0000`'s tail (`CS:0111`) — who is the visitor on a shared screen, and when that
    /// stops being true. Only the **window** decides: on the same screen but far apart, each spy
    /// sees his own view and neither console is blanked (the owner's expectation, and what the
    /// corpus shows: 831 of 1,464 same-screen rows have both flags clear).
    fn update_visitor(&mut self, i: usize) {
        if self.players < 2 {
            return;
        }
        let o = 1 - i;
        if self.spies[i].screen != self.spies[o].screen {
            return;
        }
        let host = self.spies[o].map_x;
        let x = self.spies[i].x();
        if self.spies[i].blank {
            let far_left = x <= host + VISITOR_ENTRY_LEFT;
            let far_right = x >= host + VISITOR_ENTRY_RIGHT;
            if far_left || far_right {
                self.spies[i].blank = false;
                self.spies[i].map_x = (if far_left {
                    host - VISITOR_KEEP_OFFSET
                } else {
                    host + VISITOR_KEEP_OFFSET
                })
                .clamp(0, MAP_X_MAX);
            }
        } else if !self.spies[o].blank
            && x > host + VISITOR_ENTRY_LEFT
            && x < host + VISITOR_ENTRY_RIGHT
        {
            self.spies[i].blank = true;
        }
    }

    // ---- items (stage 7) ------------------------------------------------------------------
    //
    // The original's fire is **two** buttons on one joystick, and the keyboard maps both:
    //   0x327F (button A) -> `FUN_26c0_2d22`: tap = pick up / drop / open-close the trapulator
    //   0x3283 (button B) -> `FUN_26c0_2d7e` -> `FUN_26c0_2de3`: with a direction = use, bury, throw
    // The port has one fire key per player, so a *tap* (press with no direction held this frame)
    // runs the button-A path and a *direction press while fire is held* runs the button-B path.
    // That is the mapping the owner described ("tap to take, hold and a direction to bury") and it
    // keeps the original's branch structure; the difference is only which physical key it is.

    // ---- the rocket hatch and the polar bear (FUN_26c0_2392) --------------------------------
    /// `CS:0610` — the **reveal**, at the top of the frame loop. With the launch site on HIDE the
    /// hatch's three entries are all zero (the placement skipped its write), so this is the only
    /// thing that ever puts a hatch on the level:
    ///
    /// ```text
    /// 0610  if (word[0x31D5] != 0) goto 0639       ; already in the block (SHOW, or done)
    /// 0617  if (word[0x2EB8] != 0x6C0) goto 0639    ; the case bit + all three parts
    /// 061f  [0x31B6] = 0x0F;  [.. +1] = 0x15;  [.. -1] = 0x15
    /// 0633  word[0x31D5] = 2                       ; once
    /// ```
    ///
    /// Measured over the corpus: of 11,456 sampled dumps with `0x31D5 == 0`, **none** holds a hatch
    /// byte, and exactly one holds the completed mask (`0x6C0`) with the cell still empty — the
    /// frame before the reveal. 7,578 dumps hold `0x31D5 == 2` and a `15 0F 15` triple.
    fn hatch_reveal(&mut self) {
        if self.hatch.armed != 0 || self.mask != round::MASK_HATCH_REVEAL {
            return;
        }
        let Some(addr) = self.hatch.addr else { return };
        if addr == 0 || addr + 1 >= self.ds.len() {
            return;
        }
        self.ds[addr - 1] = round::HATCH_FLANK;
        self.ds[addr] = round::HATCH_CLOSED;
        self.ds[addr + 1] = round::HATCH_FLANK;
        self.hatch.armed = 2;
        self.ds[round::DS_HATCH_ARMED..round::DS_HATCH_ARMED + 2]
            .copy_from_slice(&2u16.to_le_bytes());
    }

    /// `FUN_26c0_2392`'s second half (`CS:23cf..0x2443`) — **what opens the hatch**: the object
    /// cell under this spy's own feet (`DS:0x32D7`, `FUN_26c0_2549`), tested for the hatch family:
    ///
    /// ```text
    /// 23d3  al = [cell]; if ((al & 0x1F) > 0xC) {
    /// 23e7      if ((al & 0x1F) == 0x0F or 0x15 or 0x10 or 0x11) {
    /// 23fe          bp = other; if (other.raised && other.0x32CF >= 0x1E) return  ; busy
    /// 2417          [0x31B6] = 0x10                  ; the hatch OPENS
    /// 2420          0x2F14(this) = 0                  ; face right
    /// 2426          0x32CB(this) = 0x1E               ; the descent animation
    /// 2436          0x329B(this) = 2*entry + 2; 0x3297(this) = 0x36
    /// 2440          FUN_26c0_277c                     ; start it
    /// }}}
    /// ```
    ///
    /// The cell is the *feet* cell, i.e. the spy has to be on the hatch's own row (`y` 52..59 in
    /// row 3 of the object block), which is why the hatch reads as something you walk into. `0x11`
    /// (the bear) is in the list: the bear's cell opens the hatch exactly like the hatch's flanks.
    fn hatch_touch(&mut self, i: usize) {
        if self.hatch.seq != HatchSeq::Idle {
            return;
        }
        // `CS:71d`/`CS:731`: a launched spy has no frame at all, and an out-of-round one is skipped
        // before the hatch touch (`CS:097f`) is reached.
        if self.spies[i].launched || self.spies[i].out_of_round {
            return;
        }
        // **stage 11 — the visitor flag is NOT a gate here.** The port used to `return` for a spy
        // flagged `blank` (`0x2F7E`), reading `CS:0be7` as part of the trigger. It is not: `0be7`
        // is `FUN_26c0_0be7`, the **object layer's blitter**, and its `if (0x2f7e(si)) goto 0xc92`
        // only stops the *drawing* of a blanked console's objects. The trigger,
        // `FUN_26c0_2392`, is called unconditionally for both spies at `CS:097f` and tests exactly
        // two things — the feet cell's code and, at `CS:2404`, whether the *other* spy is already
        // mid-animation. There is no `0x2F7E` test anywhere in `0x2392..0x2443`.
        //
        // With the gate, the spy who walks into the other's window (the **visitor**, `0x2F7E` =
        // 1, and by the window rule usually the one who arrives second) could never open the
        // hatch, while the host could — the owner's "the black spy is not triggering the polar
        // bear, he just walks over the hatch. White spy does trigger it". The recordings agree
        // with the code: `playtest4` ds_01774 has the **black** spy (player 2) descending with
        // `0x2F7E`/`0x2F80` both 0, and `playtest4` ds_01813 has him entering again to launch.
        // **stage 25a — `CS:23cf` reads `DS:0x32D7`, not the live position.** The touch is the last
        // reader of the cached feet cell, so it fires on the frame *after* the spy steps onto the
        // hatch, not on the frame he arrives: `CS:944` commits the move at `0xa1a`, `FUN_26c0_2549`
        // re-caches the cell at `0xafd`, and only the next frame's `CS:2392` sees it. Measured on
        // the demo: spy 1 reaches x 116 in frame 806 and the recording's `0x32CF` only becomes 30 at
        // 807 — the port used to open the hatch in the arrival frame and led the whole
        // descent/ascent by one (`p1.y` 806, `p2.y` 822).
        let Some(addr) = self.feet_prev[i].or_else(|| self.cell_of(i)) else { return };
        let code = (self.ds[addr] & items::CODE_MASK) as u16;
        if !matches!(
            code,
            0x0F | 0x15 | 0x10 | 0x11
        ) {
            return;
        }
        let o = 1 - i;
        if self.spies[o].pose_raised && self.spies[o].pose_state >= round::STATE_DESCEND {
            return;
        }
        let Some(hatch) = self.hatch.addr else { return };
        self.ds[hatch] = round::HATCH_OPEN;

        let (entry, band) = match &self.layout.hatch {
            Some(h) => (h.entry as i32, h.band),
            None => return,
        };
        if band != self.spies[i].screen {
            return;
        }
        let s = &mut self.spies[i];
        // `CS:2420`: `0x2F14` = 0 — he faces east for the whole sequence, which is what the owner
        // sees on the way out: the descent is drawn dir 4 (into the screen, `CS:27d3`) and the
        // throw and the climb-out dir 0 (`CS:27e0`, and 0x1C keeps it).
        s.mirror = false;
        s.x3 = (2 * entry + 2) * SUB;
        s.y3 = 0x36 * SUB;
        s.dir = 4;
        s.frame = 0xFFFE; // 0x2EFC = -2
        s.frame_step = 2;
        s.pose_raised = true;
        s.pose_state = round::STATE_DESCEND;
        s.saved_set = s.set; // `277c` at `CS:27a8`; `0x32AF` is NOT written (see the field's docs)
        s.vel_x = 0;
        s.vel_y = 0;
        self.hatch.seq = HatchSeq::Descend;
        self.hatch.spy = i;
        self.hatch.frames = 0;
        self.hatch.bear_frames = 0;
        self.hatch.bear = false;
    }

    /// Advance the running hatch sequence by one game frame: the script of the current state, the
    /// state's own end test (`CS:0996`), the bear's spawn at ten frames into `0x20` (`CS:23a4`) and
    /// the hatch's close (`CS:292d`).
    ///
    /// The end test is the executable's, on its own frame word: `0x2EFC` is `2f - 2` on frame `f`,
    /// and the state ends when `0x2EFC + 2 > ceiling`, i.e. `2f > ceiling` (`CS:0996..09a2`). Stage
    /// 9a tested `2(f+1) > ceiling`, which dropped the **last** script frame of every state — for
    /// the descent that is entry 15 (clip 28, no motion) but for the throw it is `vy = +2`, so the
    /// spy landed two units short: the owner's "landing a little north".
    fn hatch_advance(&mut self) {
        // The secondary animation: the *other* spy's state 0x0C, started by `FUN_26c0_277c` from
        // `CS:2915` with the pose set from the table (`0xDC` = the laugh), direction 0 and the
        // frame word back to -2. It runs ten frames (`word[0x2CE6 + 0x0C]` = 18, `2f > 18`).
        if let Some(o) = self.hatch.other {
            let ceiling = round::state_ceiling(&self.ds_img(), 0x0C);
            let f = (self.spies[o].frame.wrapping_add(2)) / 2;
            if 2 * f > ceiling {
                let s = &mut self.spies[o];
                s.pose_raised = false;
                s.pose_state = 0;
                s.pose_clip = 0;
                s.set = s.saved_set;
                s.dir = s.saved_dir;
                s.frame = STAND_FRAME;
                s.frame_step = 0;
                self.hatch.other = None;
            } else {
                self.spies[o].frame = self.spies[o].frame.wrapping_add(2);
            }
            // his own frame tail as well (no script for 0x0C, so only his physical keys)
            self.scroll(o, self.hatch.keys[o]);
        }
        let seq = self.hatch.seq;
        if matches!(seq, HatchSeq::Idle | HatchSeq::Launched) {
            return;
        }
        let i = self.hatch.spy;
        let state = seq.state();
        let ceiling = round::state_ceiling(&self.ds_img(), state);
        let f = (self.spies[i].frame.wrapping_add(2)) / 2;
        if 2 * f > ceiling {
            self.hatch_end_state();
            return;
        }
        self.hatch.vx = 0;
        if let Some((vy, vx, clip)) = round::descent_step(&self.ds_img(), state, f) {
            self.spies[i].y3 += vy * SUB;
            self.spies[i].x3 += vx * SUB;
            self.spies[i].pose_clip = clip.clamp(0, u16::MAX as i32) as u16;
            // `CS:9ea`: `*(word[0x3293 + si]) = vx` — the script's x velocity IS the right-key flag
            // for the frame, which is what makes the throw's view scroll east with him.
            self.hatch.vx = vx;
        }
        self.spies[i].frame = self.spies[i].frame.wrapping_add(2);
        self.hatch.frames = f + 1;
        // the frame's tail: `CS:0a3e` runs `FUN_26c0_0000` for this spy too, with the right-key
        // flag the script just wrote
        let mut keys = self.hatch.keys[i];
        keys.right |= self.hatch.vx > 0;
        keys.left |= self.hatch.vx < 0;
        self.scroll(i, keys);
        if seq == HatchSeq::Bear {
            // `CS:23a0`: `++0x2F74`, and at exactly ten the bear is written into the entry two
            // to the right of the hatch's middle, saving the byte it replaces in `0x31B8`.
            self.hatch.bear_frames += 1;
            if self.hatch.bear_frames == round::BEAR_TRIGGER_FRAMES {
                if let Some(h) = self.hatch.addr {
                    if h + 2 < self.ds.len() {
                        self.hatch.saved = self.ds[h + 2];
                        self.ds[round::DS_HATCH_SAVE] = self.hatch.saved;
                        self.ds[h + 2] = round::BEAR;
                        self.hatch.bear = true;
                        // `CS:23BC..23C9`: the bear's roar, ten frames into state 0x20 — the same
                        // frame the byte goes in. 200 noise notes, 838 ms.
                        self.cue(sound::cue::HATCH_NOISE);
                    }
                }
            }
        }
    }

    /// `FUN_26c0_2807`'s dispatch (`CS:2823..0x29f2`) for the hatch's three states.
    ///
    /// The prologue (`CS:2807..2823`) runs for every state: `0x2F18 = 0` (no longer raised), the
    /// pose set back from `0x32A7`, the frame word back to 2, the direction back from `0x32AF` and
    /// the frame step to 0. Every branch below then starts the next state through `277c`, which
    /// overwrites what it needs.
    fn hatch_end_state(&mut self) {
        let i = self.hatch.spy;
        // `CS:2807..2823` — the shared prologue.
        {
            let s = &mut self.spies[i];
            s.pose_raised = false;
            s.set = s.saved_set;
            s.dir = s.saved_dir;
            s.frame = STAND_FRAME;
            s.frame_step = 0;
        }
        match self.hatch.seq {
            HatchSeq::Descend => {
                // `CS:284d`: `kit & 0x2C0 == 0x2C0` — all three parts *found* by this spy.
                let kit = self.spies[i].kit;
                if kit & round::KIT_ALL_PARTS == round::KIT_ALL_PARTS {
                    // `CS:2843..2888`: state 0x1E ends with all three parts in the kit, so the
                    // hatch is the rocket — and the 6.85 s fanfare (`0xA676`) runs under it.
                    self.cue(sound::cue::HATCH_FANFARE);
                    self.launch(i);
                    return;
                }
                // `CS:2859`: no parts → animation 0x20, and the bear is what comes out of it.
                self.hatch.seq = HatchSeq::Bear;
                self.start_spy_anim(i, round::STATE_BEAR);
                self.hatch.bear_frames = 0;
            }
            HatchSeq::Bear => {
                // `CS:2907`: mine into 0x1C (the hatch closing), the other spy into 0x0C — which
                // `277c` starts with **the pose set from the table at `0x2CC4 + 0x0C` = `0xDC`**,
                // direction 0 and the walk frame back to -2: the laugh the owner describes, and
                // what the port drew as the ordinary walk pose.
                self.hatch.seq = HatchSeq::Close;
                self.start_spy_anim(i, round::STATE_CLOSE);
                let o = 1 - i;
                let laugh = round::anim_set(&self.ds_img(), 0x0C);
                let s = &mut self.spies[o];
                s.saved_set = s.set; // `CS:27a8`, through 277c
                s.pose_raised = true;
                s.pose_state = 0x0C;
                s.set = laugh;
                s.dir = 0;
                s.frame = 0xFFFE;
                s.frame_step = 2;
                s.vel_x = 0;
                s.vel_y = 0;
                self.hatch.other = Some(o);
            }
            HatchSeq::Close => {
                // `CS:2929`: the hatch closes and the byte the bear replaced comes back.
                if let Some(h) = self.hatch.addr {
                    self.ds[h] = round::HATCH_CLOSED;
                    if h + 2 < self.ds.len() {
                        self.ds[h + 2] = self.hatch.saved;
                    }
                }
                let s = &mut self.spies[i];
                s.pose_raised = false;
                s.pose_state = 0;
                s.pose_clip = 0;
                s.set = s.saved_set;
                s.dir = s.saved_dir;
                s.frame = STAND_FRAME;
                s.frame_step = 0;
                self.hatch.seq = HatchSeq::Idle;
                self.hatch.frames = 0;
                self.hatch.bear = false;
                self.hatch.bear_frames = 0;
                self.hatch.saved = 0;
            }
            _ => {}
        }
    }

    /// `CS:2865..0x28db` — the launch: the spy had all three parts, so the hatch fires.
    ///
    /// ```text
    /// 28ae 0x2F70(si) = 1          ; the rocket is in flight: 0xc93 draws it INSTEAD of the spy
    /// 28ba 0x2F64(other) = 2       ; the other spy is out of the round
    /// 28c0 0x2F7E(other) = 1       ; ... and a visitor on the launcher's board
    /// 28c6 if 0x2F7E(si) { 0x2F7E(si) = 0; 0x24A6(si) = 0x24A6(other) }
    /// ```
    ///
    /// Measured on `playtest4` ds_01813..01816 (a spy with `kit` 0xEFF walks in): the descent runs
    /// its 16 frames, then the words above appear — `0x2F64`(other) = 2, `0x2F7E`(other) = 1 — and
    /// the spy himself is not drawn again. The ascent itself is `FUN_26c0_20ee` (stage 15); the five
    /// lines at `CS:2865` are the launch's own and are ported here, including the window copy.
    ///
    /// `CS:2865..2877` — **the launch stages the spy's picture into the rocket's window.**
    ///
    /// ```text
    /// 2865  dx = 9 (?stride)          bx = 0x32 (bottom row)      cx = 2 (column)
    /// 286e  push si; si = word[0x2E06 + si]        ; the player's staged [h][w] picture
    /// 2873  di = 0x679C                             ; the record's SECOND plane
    /// 2877  call 0x205d                             ; a rectangle copy: [h][w] then h rows
    /// ```
    ///
    /// `0x679C` is not a scratch buffer: the rocket record is `[160][9]` at `DS:0x61FA`, its AND
    /// plane is `0x61FC..0x679C` (1,440 bytes) and its **OR plane** is `0x679C..0x6D3C`, so the copy
    /// paints colour bits into the OR plane at rows `0x32 - h .. 0x31` (31..49 for the 19-row
    /// picture), columns 2..6 of the 9-byte row. Those rows' AND mask is already `0x00` (opaque) in
    /// the shipped record, so what the porthole shows is `(dest & 0) | or` — the picture itself,
    /// over a window the static record fills with a flat `0xAA` (index 2, magenta).
    ///
    /// Verified byte-for-byte: applying this copy to the pre-launch record of four recorded launches
    /// (both players) reproduces the OR plane of the launch dumps exactly — 80 of the 95 copied
    /// bytes differ from the pink they replace, the other 15 were already `0xAA`.
    fn stage_rocket_window(&mut self, i: usize) {
        let hdr = round::DS_ROCKET;
        if hdr + 2 > self.ds.len() {
            return;
        }
        let (rh, rw) = (self.ds[hdr] as usize, self.ds[hdr + 1] as usize);
        if rh == 0 || rw == 0 {
            return;
        }
        let Some(ptr) = self
            .ds
            .get(DS_STAGED_SPRITE + 2 * i..DS_STAGED_SPRITE + 2 * i + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
        else {
            return;
        };
        if ptr + 2 > self.ds.len() {
            return;
        }
        let (sh, sw) = (self.ds[ptr] as usize, self.ds[ptr + 1] as usize);
        if sh == 0 || sw == 0 || sw > rw {
            return;
        }
        let or_plane = hdr + 2 + rh * rw;
        let top = ROCKET_WINDOW_BOTTOM - sh as i32;
        if top < 0 {
            return;
        }
        for r in 0..sh {
            for c in 0..sw {
                let dst = or_plane + (top as usize + r) * rw + ROCKET_WINDOW_COL + c;
                let src = ptr + 2 + r * sw + c;
                if dst < self.ds.len() && src < self.ds.len() {
                    self.ds[dst] = self.ds[src];
                }
            }
        }
    }

    fn launch(&mut self, i: usize) {
        let o = 1 - i;
        self.hatch.seq = HatchSeq::Launched;
        self.rocket = true;
        self.rocket_asc = 0;
        // `CS:28ae` computes `0x2EBE` = `2*hcol - 0x24A6 + 2` from the hatch's own column
        // (`DS:0x320F`); the descent left this spy standing **on** that column (`CS:2436` writes
        // `x = 2*entry + 2`), so `x - 2` is the hatch's world column and the rocket needs no
        // separate word. The first ascent step is the launch frame's own (`0x2EBC` = 2 in all
        // eight recorded launch frames).
        self.rocket_x = self.spies[i].x() - 2;
        // `CS:2865..2877` — the spy's picture goes into the rocket's window before the ascent
        // starts, so the first frame that shows the window already shows him (see
        // [`Game::stage_rocket_window`]).
        self.stage_rocket_window(i);
        let was_visitor = self.spies[i].blank;
        {
            let s = &mut self.spies[i];
            s.pose_raised = false;
            s.launched = true;
            s.vel_x = 0;
            s.vel_y = 0;
        }
        self.spies[o].out_of_round = true;
        self.spies[o].blank = true;
        if was_visitor {
            self.spies[i].blank = false;
            self.spies[i].map_x = self.spies[o].map_x;
        }
    }

    /// `FUN_26c0_277c` for the hatch's own states: `0x32CF = state`, `0x2F18 = 1`, the frame word
    /// to `-2` and its step to 2; the direction 4 for the descent and 0 for the throw (`CS:27d3`,
    /// `CS:27e0`), and **untouched for 0x1C**, which keeps the throw's 0 — the owner's "comes up and
    /// out facing east". The pose set is likewise untouched for these three states; only states
    /// below `0x1E` switch it, from the `0x2CC4` table.
    fn start_spy_anim(&mut self, i: usize, state: u16) {
        self.hatch.frames = 0;
        let s = &mut self.spies[i];
        s.saved_set = s.set; // `CS:27a8` — the set, and only the set
        s.pose_raised = true;
        s.pose_state = state;
        if state == round::STATE_DESCEND {
            s.dir = 4;
        } else if state == round::STATE_BEAR {
            s.dir = 0;
        }
        s.frame = 0xFFFE;
        s.frame_step = 2;
        s.vel_x = 0;
        s.vel_y = 0;
    }

    /// The executable's data segment, for the animation tables (`0x2CC0`, `0x2CE6`).
    fn ds_img(&self) -> Vec<u8> {
        self.exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000].to_vec()
    }

    /// The object cell under a spy, as a data-segment address.
    fn cell_of(&self, i: usize) -> Option<usize> {
        let s = &self.spies[i];
        items::cell_addr(&self.ds, s.screen, s.x(), s.y())
    }
    /// The cell one entry ahead of the spy, in the direction he faces.
    fn cell_ahead_of(&self, i: usize) -> Option<usize> {
        let s = &self.spies[i];
        items::cell_ahead(&self.ds, s.screen, s.x(), s.y(), s.mirror)
    }

    fn set_pose(&mut self, i: usize, code: u16) {
        self.spies[i].set = items::code_pose(&self.exe[DS_FILE_OFF..], code);
    }

    /// Move an item into or out of the hand, keeping the kit word's bit 10 with it.
    ///
    /// `0x2D60[12]` is 0x400 and bit 10 is `[0x2F2C] == 12` on 2477/2477 recorded dumps: the
    /// original has no separate "in hand" flag, it is this bit, written where the code writes it
    /// (`CS:386e` on a pickup, `CS:3959` on a drop). Keeping the two in step here means the stow
    /// test (`CS:3191`) reads the briefcase as "already have one" and drops it, which is what the
    /// recordings do.
    fn set_carried(&mut self, i: usize, code: u16) {
        self.spies[i].carried = code;
        if code == 12 {
            self.spies[i].kit |= items::KIT_CASE_BIT;
        } else {
            self.spies[i].kit &= !items::KIT_CASE_BIT;
        }
    }

    /// `CS:383f` / `CS:387f` — the part (or the case) was taken: its position record is dropped
    /// (`word[0x3219 + 2*(code-8)] = 0xFFFF`).
    fn link_clear(&mut self, code: u16) {
        if (9..=12).contains(&code) {
            self.links[(code - 8) as usize] = cpu::Link::default();
        }
    }

    /// `CS:39b0` (and `CS:2376`) — a part or the case was put down at `addr`: record its screen,
    /// object row (`quotient`) and entry (`remainder`) of `(addr - block) / 0x9C`, and the cell.
    fn link_record(&mut self, i: usize, code: u16, addr: usize) {
        if !(9..=12).contains(&code) {
            return;
        }
        let screen = self.spies[i].screen;
        if let Some(block) = round::block_addr(&self.ds, screen) {
            if addr >= block {
                let lin = addr - block;
                self.links[(code - 8) as usize] = cpu::Link {
                    used: true,
                    screen,
                    row: (lin / 0x9C) as u8,
                    col: (lin % 0x9C) as u8,
                };
            }
        }
    }

    /// `DS:0x322D`'s write (`mov %si,ds:0x322d(%bp)`, `bp = 2*(code-8)`, `CS:383a`/`CS:3886`):
    /// the part now belongs to this player — `0` for white, `2` for black, which is the spy's own
    /// `SI` base, not a player *index*.
    fn set_part_owner(&mut self, i: usize, code: u16) {
        if let Some(slot) = items::part_owner_slot(code) {
            self.part_owner[slot] = 2 * i as u16;
        }
    }

    /// `FUN_26c0_2d22` — the fire press. Nothing in hand: try to pick something up; a second tap
    /// with nothing to take opens the trapulator. A tool in hand: latch it — the release then puts
    /// it away (`FUN_26c0_315f`). The briefcase in hand: the same pickup routine, which deposits a
    /// part into the case or swaps the case for a tool.
    fn fire_tap(&mut self, i: usize) {
        // `CS:2D22` opens with the map: `if (0x2f48 != 0) { 0x2f48 = 0; goto 0x2d6f }`, and
        // `0x2d6f` is the routine's own tail (it clears `0x2F28`, the action latch). The press is
        // therefore **consumed** — it neither picks anything up nor counts as a trapulator tap.
        if self.spies[i].map_open {
            self.spies[i].map_open = false;
            self.spies[i].act_latch = false;
            return;
        }
        let carried = self.spies[i].carried;
        if carried != CARRY_NONE && carried < 12 {
            self.spies[i].act_latch = true;
            return;
        }
        let took = self.pickup(i);
        if took {
            self.spies[i].fire_taps = 0;
            return;
        }
        if carried == CARRY_NONE {
            // 0x2d50: the first tap arms, the second opens the trapulator
            if self.spies[i].fire_taps == 1 {
                self.spies[i].trap_open = true;
                // `CS:2d22` writes the constant 1 into 0x2F38; the *sign* of that word is the
                // selection's blink phase from then on (`CS:2d1c` negates it once per frame).
                self.spies[i].trap_sign = 1;
                // the panel starts with no direction request pending
                self.spies[i].cursor_req = 0;
            } else {
                self.spies[i].fire_taps = 1;
            }
        }
    }

    /// `FUN_26c0_373b` — take (or, with the briefcase in hand, deposit into the case).
    ///
    /// Returns true when something happened. The search order is the code's: the cell under the
    /// feet, then — only if that cell is empty — one entry ahead. Codes above 12 are not items
    /// (`CMP AL,0xC; JG return`), which is what makes the hatch (`0x10`), a hole (`0x0E`) and an
    /// icicle (`0x14`) untouchable.
    fn pickup(&mut self, i: usize) -> bool {
        let Some(feet) = self.cell_of(i) else { return false };
        let mut addr = feet;
        if self.ds[addr] == 0 {
            let ahead = self.cell_ahead_of(i);
            match ahead {
                Some(a) if self.ds[a] != 0 => addr = a,
                _ => {
                    // 0x3755: the briefcase, on empty ground, arms the put-away latch instead
                    if self.spies[i].carried == 12 {
                        self.spies[i].act_latch = true;
                    }
                    return false;
                }
            }
        }
        let byte = self.ds[addr];
        let code = (byte & items::CODE_MASK) as u16;
        if code > 12 {
            return false;
        }
        let img = self.exe[DS_FILE_OFF..].to_vec();
        let img = &img[..];
        let my_tag = items::player_tag(img, i);
        let other_tag = items::player_tag(img, 1 - i);
        if byte & other_tag != 0 {
            // `CS:37ce`: the *opponent's* dynamite. The tag test comes **first** and needs no
            // `code == 8`: a stick buried on top of a part carries the part's code in its low five
            // bits (`0xCA`, measured in `m3-heat-90s`) and still goes off. Whoever taps it is the
            // one who is blown up — 0x12 on the tapper, 0x0C on the owner.
            self.blast_on_tap(i, addr);
            return true;
        }
        if byte & my_tag != 0 {
            // `CS:3794`: my own stick. With something in hand `315f` runs first (with one fire key
            // the only thing that can be here is the briefcase, `2d22`'s `>= 12` branch) and the
            // take is refused if the hand is still full.
            if self.spies[i].carried != CARRY_NONE {
                self.stow_or_drop(i);
                if self.spies[i].carried != CARRY_NONE {
                    return false;
                }
            }
            // `CS:37aa`: `& 0x1F` strips the tag and the buried bit, and the cell is cleared only
            // when what is left is a stick — so a stick lifted off a part leaves the part behind.
            let now = self.ds[addr] & items::CODE_MASK;
            self.ds[addr] = if now == items::CODE_DYNAMITE as u8 { 0 } else { now };
            self.set_carried(i, items::CODE_DYNAMITE);
            self.spies[i].act_state = 0;
            self.set_pose(i, items::CODE_DYNAMITE);
            self.spies[i].kit |= items::code_bit(img, items::CODE_DYNAMITE);
            self.item_counts[0] += 1;
            return true;
        }
        let carried = self.spies[i].carried;
        let mut cell = 0u8;
        let mut take = code;
        if carried != CARRY_NONE {
            if code >= 9 {
                // 0x3822/0x3833: the briefcase in hand and a part under the feet — the part goes
                // into the case. The mask bit is what "deposited" means, and the case stays in
                // hand (AX is forced to 12 at 0x3856).
                //
                // 0x3833-0x383a also stamps the **owner** ledger: `bp = 2*(code-8)`,
                // `ds:0x322D(bp) = SI`, `ds:0x3219(bp) = 0xFFFF` (the position record is dropped).
                let bit = items::code_bit(img, code);
                self.mask |= bit;
                self.spies[i].kit |= bit;
                self.set_part_owner(i, code);
                self.link_clear(code); // `CS:383f`
                take = 12;
                self.item_counts[6] += 1;
            } else {
                // 0x3827: any other item — the case is put down in its place and the item taken.
                cell = 12;
                self.spies[i].kit &= !items::KIT_CASE_BIT;
                // counted as the pickup below: the *item* is what entered the hand
            }
        }
        self.ds[addr] = cell;
        if cell == 12 {
            self.link_record(i, 12, addr); // `CS:3827..382d` -> `CS:39b0` with bp = 12
        }
        self.set_carried(i, take);
        if take >= 9 {
            self.link_clear(take); // `CS:387f`
            self.spies[i].kit |= items::code_bit(img, take);
            // 0x387f-0x3886: the tail every `take >= 9` shares — the part (or the case) is
            // recorded as this player's own.
            self.set_part_owner(i, take);
        }
        if take >= 12 {
            // 0x3890: the briefcase is owned, and the kit word takes the deposited mask with it
            self.spies[i].case_owned = true;
            self.spies[i].kit |= self.mask & !MASK_BASE;
            // **the transition stage 8 missed** (`CS:3896..0x38be`): taking the briefcase stamps
            // the per-spy owner ledger with the *holder's* `SI` for **every part whose deposited
            // bit is set**, so a part that was in the case stops being the other spy's:
            //
            // ```text
            // 3896  ax = word[0x2eb8]                 ; the deposited mask
            // 3899  or %ax, 0x32a3(%si)               ; my kit takes it
            // 389e  bx = 0x16                         ; 2 * code 11
            // 38a3  ax = word[0x2eb8] & word[0x2d60 + bx]  ; is that part deposited?
            // 38aa  je 0x38b6                         ; no -> next
            // 38ac  bp = bx - 0x10                    ; bp = 2 * (code - 8)
            // 38b1  mov %si, ds:0x322d(%bp)           ; THE PART IS NOW MINE
            // 38b6  bx -= 2 ; cmp $0x10, bx ; jg 0x38a3    ; codes 11, 10, 9
            // ```
            //
            // Without it the ledger keeps saying "black found the fuel tank" while white holds the
            // case with all three inside — both kit boxes then show the same part, the owner's
            // defect. Measured on the corpus (`stage11_probe.py`): **51** recorded briefcase
            // pickups, **27** of them with a part already in the case; in all 27 every deposited
            // part's ledger word reads the picker's `SI` afterwards, and in one —
            // `playtest5` ds_00897 -> ds_00898, white taking a case that holds the gyroscope and
            // the punch card — the ledger **moves** from black (2) to white (0) on that single
            // frame, which is `CS:38b1` and nothing else.
            for code in [11u16, 10, 9] {
                let bit = items::code_bit(img, code);
                if bit != 0 && self.mask & bit != 0 {
                    self.set_part_owner(i, code);
                }
            }
        }
        self.spies[i].act_state = 0;
        self.set_pose(i, take);
        // **stage 25a — `CS:38bf` is a raise, not a pose swap.**
        //
        // ```text
        // 38bf  ax = word[0x2D08 + 2*code]     ; the pose set the take leaves behind
        // 38c3  0x2EF8(si) = ax
        // 38c7  call 277c                       ; <-- FUN_26c0_277c, the animation start
        // 38ca  ... the cue
        // 38dd  call 2549                       ; the feet pointer, refreshed last
        // ```
        //
        // `277c` saves that set into `0x32A7` (`CS:27a8`), copies `0x32CB` — which the take path
        // has just written as **0** (`CS:3859`, `CS:37ba`) — into `0x32CF`, sets `0x2F18 = 1`,
        // switches the pose set to `word[0x2CC4 + 0] = 132` and seeds the frame at -2. So a spy who
        // takes, deposits or lifts his own stick is **raised** for the state-0 animation: two drawn
        // frames (`0x2CE6[0] = 2`, `CS:099e`) before `CS:2807` puts him back on the set the take
        // saved.
        //
        // The take is reached from the button-A branch of the frame body, which returns without
        // running `CS:097c`, so nothing steps the seed that frame and the raise costs **three**
        // frame bodies: the arming frame (heat already ticked), then two whole bodies with
        // `0x2F18` set, which is `CS:0727` — the heat block and the keys are both skipped. That is
        // the demo's frame 14: the recording's `0x2F54` stops dead at 14 for three frames while the
        // port kept counting, ran two ticks ahead and fired the 138-frame drain at 339 instead of
        // 341 (`STAGE24G_REPORT.md` §4).
        self.start_anim(i, round::STATE_TAKE, None);
        self.item_counts[if byte & items::BURIED != 0 { 3 } else { 0 }] += 1;
        // `CS:38CA..38D7`: the pickup's own click — one slide at div 2337, 117 ms. It shares its
        // cue with the trapulator's select, and like every cue it is dropped if one is playing.
        self.cue(sound::cue::SELECT);
        true
    }

    /// `FUN_26c0_315f` — put the thing in hand away: into the trapulator if the kit has a slot for
    /// it, on the ground if not. `table[carried] == carried` is the ownership test: the opponent's
    /// plunger or bucket has no slot on this side, so it is dropped.
    fn stow_or_drop(&mut self, i: usize) {
        let carried = self.spies[i].carried;
        if carried == CARRY_NONE {
            return;
        }
        self.spies[i].act_latch = false;
        let img_off = DS_FILE_OFF;
        let canon = items::canon_code(&self.exe[img_off..], i, carried);
        if canon != carried {
            self.drop_item(i, false);
            return;
        }
        if carried == items::CODE_DYNAMITE {
            // `CS:3183`: the trapulator holds fifteen sticks (`DS:0x330F`); at fifteen there is no
            // room and the stick goes on the ground instead.
            if self.spies[i].dyn_sticks >= items::DYN_MAX {
                self.drop_item(i, false);
                return;
            }
            self.spies[i].dyn_sticks += 1;
        } else {
            // `CS:3191-319a`: the kit already holding this tool is what sends it to the ground
            // (`JE 0x31a5` is the *stow* branch, so a clear bit stows and a set bit drops). The
            // trapulator stores one of each; a second saw in hand has nowhere to go.
            let bit = items::code_bit(&self.exe[img_off..], carried);
            if self.spies[i].kit & bit != 0 {
                self.drop_item(i, false);
                return;
            }
        }
        let cursor = items::code_cursor(&self.exe[img_off..], carried);
        self.spies[i].trap_cursor = cursor;
        self.spies[i].kit |= items::code_bit(&self.exe[img_off..], carried);
        self.set_carried(i, CARRY_NONE);
        self.spies[i].act_state = 0;
        self.set_pose(i, CARRY_NONE);
        self.item_counts[4] += 1;
    }

    /// Would `FUN_26c0_315f` leave this spy's hand empty? The same decisions as
    /// [`Game::stow_or_drop`], without doing them: a tool the kit has room for is stowed; anything
    /// else is dropped (`CS:38e1`), and a drop is refused indoors, above y = 0x24, or with no free
    /// cell at the feet / one entry either side. The computer reads the hand on the instruction
    /// after the call (`CS:1ad0`, `CS:1b9d`).
    pub fn stow_empties_hand(&self, i: usize) -> bool {
        let carried = self.spies[i].carried;
        if carried == CARRY_NONE {
            return true;
        }
        let img = &self.exe[DS_FILE_OFF..];
        let canon = items::canon_code(img, i, carried);
        let mut drops = canon != carried;
        if !drops {
            if carried == items::CODE_DYNAMITE {
                drops = self.spies[i].dyn_sticks >= items::DYN_MAX;
            } else {
                drops = self.spies[i].kit & items::code_bit(img, carried) != 0;
            }
        }
        if !drops {
            return true;
        }
        let (screen, x, y) = {
            let s = &self.spies[i];
            (s.screen, s.x(), s.y())
        };
        if screen >= FIRST_INTERIOR_SCREEN || y < 0x24 {
            return false;
        }
        let Some(feet) = items::cell_addr(&self.ds, screen, x, y) else { return false };
        [Some(feet), feet.checked_sub(1), Some(feet + 1)]
            .into_iter()
            .flatten()
            .any(|a| a < self.ds.len() && self.ds[a] == 0)
    }

    /// `FUN_26c0_38e1` — put the carried code on the ground. `bury` is the routine's `BX`: 1 sets
    /// bit 7 (hidden under a mound), and a buried *stick* also takes the player's owner tag.
    ///
    /// The guards are the code's own: nothing in hand, no drop indoors (`screen >= 0x0A`), none
    /// above `y = 0x24`. The landing cell is the feet; if that is occupied the code steps west and
    /// then east. Only a **buried stick** may land on an occupied cell, and only when the byte
    /// already there carries no owner tag (`CS:3930`'s `and $0x60` / `je 0x394c`): a stick on a
    /// part is a trap on the part, a stick on somebody's stick is not allowed and steps aside.
    fn drop_item(&mut self, i: usize, bury: bool) -> bool {
        let carried = self.spies[i].carried;
        if carried == CARRY_NONE {
            return false;
        }
        let (screen, x, y) = {
            let s = &self.spies[i];
            (s.screen, s.x(), s.y())
        };
        if screen >= FIRST_INTERIOR_SCREEN || y < 0x24 {
            return false;
        }
        let Some(feet) = items::cell_addr(&self.ds, screen, x, y) else { return false };
        let mut addr = feet;
        if self.ds[feet] != 0 {
            let stackable = carried == items::CODE_DYNAMITE && bury
                && self.ds[feet] & items::OWNER_TAGS == 0;
            if !stackable {
                let west = feet.checked_sub(1);
                let east = feet + 1;
                if let Some(w) = west {
                    if self.ds[w] == 0 {
                        addr = w;
                    } else if east < self.ds.len() && self.ds[east] == 0 {
                        addr = east;
                    } else {
                        return false;
                    }
                } else if east < self.ds.len() && self.ds[east] == 0 {
                    addr = east;
                } else {
                    return false;
                }
            }
        }
        let img = self.exe[DS_FILE_OFF..].to_vec();
        if carried >= 9 {
            // 0x394c -> 0x39b0: the parts and the case keep a position record and toggle their
            // kit bit (dropping a part makes it "found but hidden", which is the kit box's
            // non-flashing state).
            let bit = items::code_bit(&img, carried);
            self.spies[i].kit ^= bit;
        }
        if carried >= 12 {
            // 0x3959: with the case on the ground the part mirrors and the in-hand bit go
            self.spies[i].kit &= items::KIT_TOOLS;
        }
        // `CS:3938` zeroes `AX` before it searches west and east, so a stick that has to step aside
        // is written as plain `8 | tag | 0x80`; only the cell it was standing on can carry an
        // object for the stick to sit on.
        let base = if addr == feet { self.ds[feet] } else { 0 };
        let mut byte = carried as u8;
        if carried == items::CODE_DYNAMITE && bury {
            // `CS:3969`: a buried stick keeps whatever object it was set *on* (that is "dynamite
            // as a trap on an object"), or is plain dynamite when the cell is empty, and always
            // takes the owner tag.
            let on_top = if base != 0 { base } else { carried as u8 };
            byte = on_top | items::player_tag(&img, i) | items::BURIED;
        } else if bury {
            byte |= items::BURIED;
        }
        self.ds[addr] = byte;
        if carried >= 9 {
            self.link_record(i, carried, addr); // `CS:394c` -> `CS:39b0`
        }
        self.set_carried(i, CARRY_NONE);
        self.spies[i].act_state = 0;
        self.set_pose(i, CARRY_NONE);
        self.item_counts[if bury { 2 } else { 1 }] += 1;
        // `CS:398E..399B`: the byte goes into the ice and the put-down cue runs — 18 one-tick
        // slides, 79 ms.
        self.cue(sound::cue::PUTDOWN);
        true
    }

    /// `FUN_26c0_2d7e` + `FUN_26c0_2de3` — fire held, a direction pressed.
    ///
    /// Nothing in hand: the snowball (`FUN_26c0_2f87`, only when both spies are on one screen).
    /// A tool: its own action. A part or the case: bury it (`38e1` with `BX = 1`).
    ///
    /// `FUN_26c0_2de3`, branch for branch (`CS:2de3`..`2f86`):
    ///
    /// ```text
    /// 2df4  ax = carried
    /// 2df8  if (carried == word[0x2DB2+si])   ; MY plunger (1 white / 2 black) -> detonate; 13b
    /// 2e64  else if (carried == 3)            ; the saw
    /// 2e69      di = *(0x32D7+si)             ; the feet cell
    /// 2e6d      di += (0x2F14 ? -1 : +1)      ; ONE ENTRY IN FRONT — the way he faces
    /// 2e79      if (*di != 0) return          ; the target must be empty
    /// 2e81      ax = FUN_366e(y, x, band)     ; the terrain probe, at the SPY's own cell
    /// 2e90      if ax not in (0, 0x10, 0x11, 5) return
    /// 2ea7      *(0x331F+si) = di             ; remember the cell for the finish
    /// 2eab      0x32CB = 4                    ; the sawing animation (12 frames)
    /// 2ec7  else if (carried == word[0x2DB6+si]) ; MY bucket (4 white / 5 black)
    ///           ... the same six steps, state 8, 12 frames ...
    /// 2f2b  else if (carried == 6)            ; the ice pick
    /// 2f32      ax = FUN_366e(y, x, band); if (ax != 0xA) return   ; only in the door arch
    /// 2f46      di = word[0x2600 + band] + (x >> 1) - 1             ; NO ROW TERM: row 0
    /// 2f55      3 x *di++ = 0x14
    /// 2f5b      0x2F6C = 1                    ; do not knock MYSELF out with it
    /// 2f61      0x32CB = 6                    ; state 6 (6 frames)
    /// 2f80  else  bx = 1; call 0x38e1          ; bury
    /// ```
    ///
    /// The tool is **not** consumed: nothing in these branches touches the hand, and the
    /// recordings say the same (the saw is still in hand 30 frames after the hole appears,
    /// `playtest2` ds_00633; the bucket in `autoplay-bucket-trap-2`; the ice pick in `iceaxe`).
    fn directed_action(&mut self, i: usize, dir_edge: [bool; 4]) -> bool {
        // `CS:2de3`'s own head: `0x2FAA` is the terrain code the last probe returned — `FUN_26c0_366e`
        // stores its result there (`CS:36eb`) and the frame body has just probed the spy's own cell
        // (`CS:096a`). A climb (1 or 0x17) refuses **every** directed action, the bury included.
        {
            let (screen, x, y) = {
                let s = &self.spies[i];
                (s.screen, s.x(), s.y())
            };
            if matches!(
                self.terrain.code(&self.levels, screen, x, y),
                Some(terrain::T_CLIMB) | Some(terrain::T_CLIMB_HI)
            ) {
                return true;
            }
        }
        let carried = self.spies[i].carried;
        if carried == CARRY_NONE {
            self.throw_snowball(i, dir_edge);
            return true;
        }
        let img = self.exe[DS_FILE_OFF..].to_vec();
        let img = &img[..];
        let (plunger, bucket) = (items::slot_code(img, 0, i), items::slot_code(img, 4, i));
        if carried == plunger {
            // `CS:2dfe..2e61`: the detonator — the opponent's cell, my tag.
            self.detonate(i);
            return true;
        }
        if carried == items::CODE_SAW {
            if let Some(cell) = self.tool_target(i) {
                // `CS:2EA7..2EBE`: the saw's 1.8 s of one-tick slides.
                self.cue(sound::cue::SAW);
                self.start_anim(i, round::STATE_SAW, Some(cell));
            }
        } else if carried == bucket {
            if let Some(cell) = self.tool_target(i) {
                // `CS:2F15..2F22`: the bucket's 56 alternating notes.
                self.cue(sound::cue::BUCKET);
                self.start_anim(i, round::STATE_BUCKET, Some(cell));
            }
        } else if carried == items::CODE_ICE_PICK {
            self.ice_pick(i);
        } else {
            self.drop_item(i, true);
        }
        true
    }

    /// `CS:2dfe..2e61` — **the detonator**. Written out from the disassembly:

    /// ```text
    /// 2dfe  bx = si; si = word[0x32D3+si]        ; the OTHER spy
    /// 2e06  di = word[0x32D7+si]                 ; HIS cached cell — not mine, not "in front"
    /// 2e0a  ax = *di & word[0x3307+bx]           ; my owner tag (0x20 white / 0x40 black)
    /// 2e10  if (ax == 0) goto 0x2e46             ; nothing of mine under him: a DUD
    /// 2e12  ax = *di; al &= 0x1F
    /// 2e1a  if (al == 8) *di = 0                 ; a stick leaves the ice…
    /// 2e21  sound -5; cue 0xA3E2
    /// 2e27  0x32CB(victim) = 0x12                ; the explosion, 36 frames, pose 0x1B4
    /// 2e2d  call 0x315f                          ; …on the VICTIM: he puts away what he holds
    /// 2e30  call 0x277c                          ; start his animation
    /// 2e46  (dud or not)  0x32CB(firer) = 0x0A   ; the four-frame plunger pose, pose 0xD4
    /// 2e4e  cue 0xA330
    /// ```
    ///
    /// Three things fall out of that shape and each one is a rule worth stating: the check is
    /// **my tag, not "a stick"** (so the other spy's stick under him is a dud); the byte is cleared
    /// only when its *low five bits* are 8 (so a stick buried on top of a part — `0xCA` — explodes
    /// and stays, a trap that can be fired again); and the firer pays state 0x0A **even on a dud**,
    /// with no laugh for the victim (the laugh at `CS:2480` belongs to the *hazards*, not to this
    /// branch). The plunger is not consumed.
    fn detonate(&mut self, i: usize) -> bool {
        let img = self.exe[DS_FILE_OFF..].to_vec();
        let o = items::other_player(&img, i);
        let tag = items::player_tag(&img, i);
        if let Some(cell) = self.cell_of(o) {
            let byte = self.ds[cell];
            if byte & tag != 0 {
                if byte & items::CODE_MASK == items::CODE_DYNAMITE as u8 {
                    self.ds[cell] = 0;
                    self.item_counts[9] += 1; // a stick blown off the ice
                }
                // `CS:2e21` — **the blast costs the victim 10 cold** (`mov $0xFFFB,%ax`), between
                // the byte clear and the state word. Stage 13b read it as "sound -5"; `21bd` has
                // no sound call in it, and the cue (`0x3496 = 0xA3E2`) is a separate write at
                // `CS:2e40`.
                self.heat_step(o, -DAMAGE_BLAST_STEPS);
                // `CS:2E33..2E40`: the explosion cue. 156 noise notes, 654 ms.
                self.cue(sound::cue::EXPLOSION);
                // `2e27` writes the state word first, `2e2d` puts the hand away, `2e30` starts the
                // animation. `315f` clears `0x32CB`, so the state is written again after it — and
                // again after `277c`, which refuses when the victim is already animating.
                self.spies[o].act_state = round::STATE_BLAST;
                self.stow_or_drop(o);
                self.spies[o].act_state = round::STATE_BLAST;
                // **stage 24f** — the blast is the other animation the code starts on the *other*
                // spy (`CS:2e30` inside the firer's body), so its seed frame is spent by the draw:
                // [`Game::draw_clamp`].
                self.start_anim(o, round::STATE_BLAST, None);
                self.spies[o].act_state = round::STATE_BLAST;
            }
        }
        self.spies[i].act_state = round::STATE_PLUNGER;
        self.start_anim(i, round::STATE_PLUNGER, None);
        self.spies[i].act_state = round::STATE_PLUNGER;
        self.item_counts[10] += 1;
        // `CS:2E4E..2E5B`: the plunger's own thunk, fired on a dud too.
        self.cue(sound::cue::BUCKET);
        true
    }

    /// `CS:37ce` — the *other* way into the same explosion: tapping fire (button A) while standing
    /// on the other spy's stick, with an empty hand and no plunger at all.
    ///
    /// ```text
    /// 37ce  *di &= 0x1F; if ((*di & 0x1F) == 8) *di = 0
    /// 37de  sound -5; cue 0xA3E2
    /// 37e4  0x32CB(TAPPER) = 0x12; 315f; 277c; cue 0xA3E2
    /// 3803  si ^= 2; 0x32CB = 0x0C; 277c         ; the OWNER laughs
    /// ```
    ///
    /// So the tapper blows **himself** up and the owner laughs — the mirror image of the plunger,
    /// where the firer is safe and gets 0x0A instead of a laugh.
    fn blast_on_tap(&mut self, i: usize, cell: usize) {
        let o = items::other_player(&self.exe[DS_FILE_OFF..], i);
        self.ds[cell] &= items::CODE_MASK;
        if self.ds[cell] & items::CODE_MASK == items::CODE_DYNAMITE as u8 {
            self.ds[cell] = 0;
            self.item_counts[9] += 1;
        }
        self.spies[i].act_state = round::STATE_BLAST;
        // `CS:37de` — the tapper's own 10 cold, right after the byte is stripped and before the
        // state word. Stage 13b read this `mov $0xFFFB,%ax` as "sound -5".
        self.heat_step(i, -DAMAGE_BLAST_STEPS);
        self.stow_or_drop(i);
        self.spies[i].act_state = round::STATE_BLAST;
        self.start_anim(i, round::STATE_BLAST, None);
        self.spies[i].act_state = round::STATE_BLAST;
        // `CS:37DE` and `CS:37F7` both write 0xA3E2; the second is dropped by the queue, so one
        // call is the original's own behaviour.
        self.cue(sound::cue::EXPLOSION);
        self.spy_laugh(o);
    }

    /// The saw's and the bucket's shared first half (`CS:2e69..2ea4` / `CS:2ecd..2f06`).
    ///
    /// The target is the feet cell stepped **one entry in the facing direction** and it must be
    /// empty. The terrain veto is then taken at the **spy's own** cell (`bx/cx/bp` =
    /// `0x3297/0x329B/0x329F`, not the target's) and accepts 0, 5, 0x10 or 0x11 — a quirk that
    /// matters on the boundary between two cells and is kept as read.
    fn tool_target(&mut self, i: usize) -> Option<usize> {
        let feet = self.cell_of(i)?;
        let target = if self.spies[i].mirror {
            feet.checked_sub(1)?
        } else {
            feet + 1
        };
        if target >= self.ds.len() || self.ds[target] != 0 {
            return None;
        }
        let (screen, x, y) = {
            let s = &self.spies[i];
            (s.screen, s.x(), s.y())
        };
        let t = self.terrain.code(&self.levels, screen, x, y)?;
        if !terrain::T_CUTTABLE.contains(&t) {
            return None;
        }
        Some(target)
    }

    /// `CS:2f2b..2f7d` — the ice pick. Needs the door arch under the spy (`T_DOOR_ARCH`), writes
    /// three `0x14` bytes into **row 0** of the band's block at `(x >> 1) - 1 .. +1`, and sets the
    /// latch that keeps the icicle off its own placer.
    ///
    /// The row term is genuinely absent from the write (`CS:2f4d` adds `word[0x2600 + bp]` and
    /// nothing else), so the icicle always hangs on the room's **front row** — the row a spy uses
    /// when he walks in through the door. The three bytes overwrite whatever was there; there is no
    /// emptiness test on this branch.
    fn ice_pick(&mut self, i: usize) {
        let (screen, x, y) = {
            let s = &self.spies[i];
            (s.screen, s.x(), s.y())
        };
        if self.terrain.code(&self.levels, screen, x, y) != Some(terrain::T_DOOR_ARCH) {
            return;
        }
        let img = self.exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000].to_vec();
        let Some(base) = round::block_addr(&img, screen) else { return };
        let entry = (x >> 1).max(0) as usize;
        if entry == 0 {
            return;
        }
        let addr = base + entry - 1;
        for k in 0..3 {
            if addr + k < self.ds.len() {
                self.ds[addr + k] = items::OBJ_ICICLE;
            }
        }
        self.spies[i].icicle_latch = true;
        // `CS:2F67..2F74`: the pick's own thud, 16 ticks.
        self.cue(sound::cue::PICK);
        self.start_anim(i, round::STATE_ICE_PICK, None);
    }

    /// The slot of the spy whose console owns the room — the **host** (`0x2F7E` clear), which is
    /// the one `FUN_26c0_207b` draws the icicle for (`CS:2500..250b`).
    fn host_slot(&self, i: usize) -> usize {
        if self.spies[i].blank { 1 - i } else { i }
    }

    /// `FUN_26c0_277c` — start an animation state, unless one is already running.
    ///
    /// ```text
    /// 277f  if (0x2F18) { only 0x16/0x18 keep going; else return }   ; already animating
    /// 27a1  if (0x2F64 > 1) return                                    ; out of the round
    /// 27a8  0x32A7 = 0x2EF8                                           ; save the pose set
    /// 27b0  0x2F18 = 1; 0x32CF = 0x32CB                               ; running
    /// 27c5  if (state < 0x1E) 0x2EF8 = word[0x2CC4 + state]           ; the pose set
    /// 27f7  0x2EFC = -2; 0x2F08 = 2
    /// ```
    ///
    /// **The save at `27a8` is on the `0x2F18 == 0` branch only.** The knock-out's preemption
    /// (`CS:2786..279c`) jumps straight to `0x27b0`, so it does **not** overwrite the saved set: a
    /// spy knocked out of a throw comes back to the set that was saved when the *throw* began, i.e.
    /// the walk set. Stage 19a's fix — saving unconditionally left the victim's `0x2EF8` pointing
    /// at the throw's own pose block (`0x88`) for the rest of the round, which is the owner's
    /// "frozen copy of the action sprite as the upper body, legs still walking".
    fn start_anim(&mut self, i: usize, state: u16, cell: Option<usize>) -> bool {
        if self.spies[i].out_of_round {
            return false;
        }
        // `0x2F18` is one word for the hatch's climb and for these animations; the port keeps the
        // hatch in its own state machine, so both have to be checked here.
        let already_running = self.spies[i].anim.is_some() || self.spies[i].pose_raised;
        if already_running {
            // `CS:2786..279c`: while a state runs, the only states that may take over are the two
            // snowball knock-outs, and only from `0x32CF <= 2` (a signed compare, so -1 counts) —
            // i.e. from the throw (2) or from nothing. A spy in the saw, a hazard or the hatch
            // keeps his animation, and `0x32CF` is left holding whatever `CS:30c9` wrote (-1).
            let ball_ko = matches!(state, round::STATE_KNOCK_FACE | round::STATE_KNOCK_BACK);
            let current = self.spies[i].pose_state as i16; // `0x32CF`, a signed compare
            if !(ball_ko && current <= 2) {
                return false;
            }
        }
        if self.hatch.seq != HatchSeq::Idle
            && (self.hatch.spy == i || self.hatch.other == Some(i))
        {
            return false;
        }
        let img = self.exe[DS_FILE_OFF..].to_vec();
        let s = &mut self.spies[i];
        // `CS:27a8` — the set is saved only when the animation starts from a *resting* spy. A
        // knock-out that preempts a running action jumps to `0x27b0` and leaves `0x32A7` holding
        // the set the preempted action itself saved (the walk set), so the victim's `0x2EF8` comes
        // back to the walk when the two knock-out frames are over.
        if !already_running {
            s.saved_set = s.set;
        }
        s.pose_raised = true;
        s.pose_state = state;
        s.act_state = state;
        if state < round::STATE_DESCEND {
            s.set = round::anim_set(&img, state);
            s.dir = 0;
        }
        s.frame = round::SEED_FRAME;
        s.frame_step = 2;
        s.vel_x = 0;
        s.vel_y = 0;
        s.anim = Some(Anim { state, frame: -2, cell });
        // **stage 24f** — if this spy's own body has already run this frame, nobody will step that
        // seed: the draw's `CS:0d44` spends it (see [`Game::draw_clamp`]).
        self.seed_pending[i] = self.body_ran.get(i).copied().unwrap_or(false);
        true
    }

    /// `CS:0d44` — **the draw spends an animation's seed frame**, and that is one frame off the
    /// reaction.
    ///
    /// ```text
    /// d44  ax = 0x2EFC(spy)
    /// d4a  if (ax >= 0) goto d53          ; already stepped this frame: nothing to do
    /// d4c  0x2EFC(spy) = 0                ; the seed frame is spent by the DRAW, not by a step
    /// ```
    ///
    /// `FUN_26c0_277c` seeds every animation at `0x2EFC = -2`. A spy's **own** body steps that seed
    /// in the same frame (`CS:0982` → `CS:098c`), so by the time the draw reaches him the word is
    /// already `0` and the clamp never bites. But the two animations the code starts on the
    /// **other** spy — the reaction laugh (`CS:2480`, `0x24de`, `0x2511`) and the plunger's blast
    /// (`CS:2e30`) — are armed from a body that has already run, so nothing steps them that frame,
    /// the draw's clamp is the only thing that touches them, and the first *stepped* frame is 2, not
    /// 0: the reaction is one frame shorter and the spy is back on his keys a frame earlier.
    ///
    /// Measured on `demo-full-24c`: `ds_00400` — the frame spy 1 falls into the saw hole, and the
    /// frame spy 2 is armed with the laugh — holds `0x2EFE = 0` (`0xFFFE` is what a `-2` seed reads),
    /// and spy 2 walks again at 602, where a seed-frame model kept him raised until 603 and cost him
    /// the last `down` frame of the recording's key window (frames 591..607), which put him one step
    /// high at 608 and then on his friend's row at 613.
    ///
    /// The draw reaches **one spy per console** (`CS:0b54` → `CS:0c93`, `BP` being that console's
    /// own spy or the visitor the board table at `0x31A2` hands it), and the frame loop calls it
    /// once for each console (`SI = 2` then `0`), so across the two calls both spies are spent. A
    /// spy who is out of the round (`0x2F64 >= 2`, `CS:0cba`) or in the rocket (`0x2F70`,
    /// `CS:0cc5`) is not drawn, so nothing is spent for him.
    ///
    /// Which spies it bites is [`Game::seed_pending`]: an animation armed **after** its owner's body
    /// ran (`CS:0702` runs player 2 first) — the reaction laugh, the plunger's blast, and the
    /// knock-out a ball lands. An animation armed inside the owner's own body is stepped there and
    /// never comes here, and neither does one armed on a spy whose body is still to come (his own
    /// `CS:098c` steps the seed, which is the same frame's worth of work).
    fn draw_clamp(&mut self, i: usize) {
        if !self.seed_pending.get(i).copied().unwrap_or(false) {
            return;
        }
        self.seed_pending[i] = false;
        let drawn = {
            let s = &self.spies[i];
            !s.out_of_round && !s.launched && s.frozen < 2
        };
        if !drawn {
            return;
        }
        if let Some(a) = &mut self.spies[i].anim {
            if a.frame < 0 {
                a.frame = 0;
            }
        }
        if (self.spies[i].frame as i16) < 0 {
            self.spies[i].frame = 0;
        }
    }

    /// `CS:098c..0a2e` — one frame of a running animation: `frame += 2`, and the state ends on the
    /// first step that would pass `word[0x2CE6 + state]`. While an animation runs the frame body
    /// never reads a key (`CS:0727`), which is why the spy stands still for its whole duration.
    fn advance_anim(&mut self, i: usize) {
        let Some(a) = self.spies[i].anim else { return };
        // a step spends the seed by itself
        self.seed_pending[i] = false;
        let img = self.exe[DS_FILE_OFF..].to_vec();
        let ceiling = round::state_ceiling(&img, a.state);
        let next = a.frame.wrapping_add(2);
        if next as i32 > ceiling as i32 {
            self.finish_anim(i);
            return;
        }
        // `CS:09aa..09b8`: the release test at `9b1` reads the **stored** frame word, and the
        // increment (`0xa2e`, `0x2EFC += 0x2F08`) happens *after* it — so the ball leaves on the
        // frame after `0x2EFC` first reaches 8, i.e. the frame that writes 10, and that same
        // frame's draw pass moves it once (`CS:e74`). The first column ever tested is therefore the
        // launch column plus one velocity, but the release is one frame later than "the frame that
        // writes 8". Recorded: `playtest5` `ds_00820` holds `0x2EFC = 8` with `0x2F40 = 0` and the
        // column still on the launch column; `ds_00821` holds 10 with the ball one step in and
        // already gone (stage 21a).
        if a.state == round::STATE_THROW && a.frame == BALL_LAUNCH_FRAME {
            if let Some(b) = &mut self.spies[i].ball {
                b.flying = true;
            }
        }
        self.spies[i].anim = Some(Anim { frame: next, ..a });
        self.spies[i].frame = next as u16;
        self.spies[i].frame_step = 2;
    }

    /// `FUN_26c0_2807` — the end of an animation: the shared prologue, then the state's own tail.
    ///
    /// ```text
    /// 2807  0x2F18 = 0; 0x2EF8 = 0x32A7; 0x2EFC = 2; 0x2F0C = 0x32AF; 0x2F08 = 0
    /// 2829  if (state == 0x1A) 0x2F44(host) = 0        ; the icicle over the door goes away
    /// 28de  if (state == 4) *(0x331F) = 0x0E           ; THE HOLE, twelve frames after the cut
    /// 28ef  if (state == 8) *(0x331F) = 0x0D           ; THE SPILL, twelve frames after the spill
    /// ```
    ///
    /// `0x32CF` is *not* cleared here (only the hatch's `0x1C` clears it, `CS:293a`), so a finished
    /// animation leaves its state word behind — which is why a recorded dump can show `act 0x0C`
    /// with nobody animating.
    fn finish_anim(&mut self, i: usize) {
        let Some(a) = self.spies[i].anim else { return };
        self.seed_pending[i] = false;
        {
            let s = &mut self.spies[i];
            s.anim = None;
            s.pose_raised = false;
            s.set = s.saved_set;
            s.dir = s.saved_dir;
            s.frame = STAND_FRAME;
            s.frame_step = 0;
        }
        match a.state {
            round::STATE_SAW | round::STATE_BUCKET => {
                let byte = if a.state == round::STATE_SAW {
                    items::OBJ_HOLE
                } else {
                    items::OBJ_SPILL
                };
                if let Some(c) = a.cell {
                    if c < self.ds.len() {
                        self.ds[c] = byte;
                        self.item_counts[7] += 1;
                    }
                }
            }
            round::STATE_KNOCKOUT => {
                let host = self.host_slot(i);
                self.spies[host].icicle_hit = false;
            }
            // `CS:2946..296e` — **the freeze is the only animation that ends a spy's round.** The
            // `0x2F64` word becomes 2 (out of the round) and the room is handed over: if the other
            // spy is the visitor he takes my `map_x` (`0x2960`), I become the visitor (`0x2968`),
            // and unless he is out too he becomes the host of this room (`0x2975`).
            round::STATE_FREEZE => {
                self.spies[i].frozen = 2;
                self.spies[i].out_of_round = true;
                let o = 1 - i;
                if self.spies[o].blank {
                    self.spies[o].map_x = self.spies[i].map_x;
                }
                self.spies[i].blank = true;
                if self.spies[o].frozen != 2 && !self.spies[o].out_of_round {
                    self.spies[o].blank = false;
                }
            }
            _ => {}
        }
    }

    /// The other spy's laugh (`CS:2480`, `0x24de`, `0x2511`): state `0x0C`, pose set `0xDC`, ten
    /// frames. He only laughs if he is idle — `277c` refuses while an animation runs, and the
    /// original writes `0x32CB` before the call regardless.
    ///
    /// **Stage 24f** — the laugh is one of the two animations the code starts on the *other* spy, so
    /// its seed frame is spent by the draw rather than by a step ([`Game::draw_clamp`]): one frame of
    /// the reaction, which is what the demo's key window measures.
    fn spy_laugh(&mut self, o: usize) {
        self.spies[o].act_state = round::STATE_LAUGH;
        self.start_anim(o, round::STATE_LAUGH, None);
    }

    /// `FUN_26c0_2444` — the hazard under the feet, resolved once per frame **after** the keys and
    /// **before** the move (`CS:0836`).
    ///
    /// ```text
    /// 2444  di = *(0x32D7+si)                   ; the cached feet cell
    /// 2448  if (*di == 0) { 0x2F6C = 0; return } ; empty ground clears the latch
    /// 2457  if (0x2F6C) return                   ; the placer's own icicle cannot hit him
    /// 2463  ax = *di & 0x1F; if (ax <= 0xC) return
    /// 2473  0x0D -> *di = 0; other -> 0x0C (laugh); me -> 0x0E (slip, 40 frames);
    ///               cue 0xa594; 315f (put away); 277c
    /// 24ac  0x0E -> *di = 0; sound -3; me -> 0x10 (fall, 38 frames); cue 0xa5e6; 315f; 277c;
    ///               other -> 0x0C
    /// 24eb  0x14 -> clear 3 bytes from di-1; 0x2F44(host) = 1; sound -5; cue 0xa3e2;
    ///               other -> 0x0C; me -> 0x1A (knock-out, 36 frames); 0x3297 = 0x26
    /// ```
    ///
    /// There is **no ownership test**: a spy's own spill trips him as readily as the other's, and
    /// the only immunity in the code is the `0x2F6C` latch above.
    fn hazard_check(&mut self, i: usize) {
        let Some(cell) = self.cell_of(i) else { return };
        let byte = self.ds[cell];
        if byte == 0 {
            self.spies[i].icicle_latch = false;
            return;
        }
        if self.spies[i].icicle_latch {
            return;
        }
        let code = (byte & items::CODE_MASK) as u16;
        if code <= items::CODE_LAST_ITEM {
            return;
        }
        let o = 1 - i;
        match code {
            0x0D => {
                self.ds[cell] = 0;
                self.spy_laugh(o);
                self.stow_or_drop(i);
                // `CS:2490..249D`: the spilled-bucket slip — a 36-step *rising* glissando. Rising
                // for a spill and falling for the hole is the original's own choice, not a guess.
                self.cue(sound::cue::SLIP_RISE);
                self.start_anim(i, round::STATE_SLIP, None);
                self.item_counts[8] += 1;
            }
            0x0E => {
                self.ds[cell] = 0;
                // `CS:24b4` — **the fall costs 6 cold**, between the byte clear and the state
                // word: `mov $0xFFFD,%ax; call 0x21bd` on the victim. (Stage 13a recorded the
                // same address as "sound -3"; `21bd` has no sound call in it.)
                self.heat_step(i, -DAMAGE_HOLE_STEPS);
                self.stow_or_drop(i);
                // `CS:24C0..24CD`: the hole — the falling sweep.
                self.cue(sound::cue::FALL);
                self.start_anim(i, round::STATE_FALL, None);
                self.spy_laugh(o);
                self.item_counts[8] += 1;
            }
            0x14 => {
                let start = cell.saturating_sub(1);
                for k in 0..3 {
                    if start + k < self.ds.len() {
                        self.ds[start + k] = 0;
                    }
                }
                let host = self.host_slot(i);
                // `CS:2511` — **the icicle costs 10 cold**: `mov $0xFFFB,%ax; call 0x21bd`,
                // after the three bytes and the host's `0x2F44` flag, before the cue.
                self.heat_step(i, -DAMAGE_ICICLE_STEPS);
                // `CS:2517..2524`: the icicle's blast.
                self.cue(sound::cue::EXPLOSION);
                self.spies[host].icicle_hit = true;
                self.spy_laugh(o);
                self.start_anim(i, round::STATE_KNOCKOUT, None);
                // `CS:2542`: the knock-out drops him onto the next row (y = 0x26 = 38).
                self.spies[i].y3 = 0x26 * SUB;
                self.item_counts[8] += 1;
            }
            _ => {}
        }
    }

    /// `FUN_26c0_2f87` — **the snowball**, reached from `FUN_26c0_2d7e` (button B) with an empty
    /// hand. Written out from the disassembly, gate for gate:
    ///
    /// ```text
    /// 2db6  (2d7e, empty hand) if (0x2F7E + 0x2F80 == 0) return   ; both spies must share a screen
    /// 2dc5                 if (other.0x2F64 >= 1) return          ; he is frozen / out of the round
    /// 2f8a  (2f87)         if (0x2F7E + 0x2F80 == 0) return
    /// 2f99                 if (other.0x2F64 != 0) return
    /// 2fa4                 if (0x2F40) return                     ; a ball of mine is already up
    /// 2fab                 if (0x2FAA in {1, 0x17, 2, 3}) return  ; climb / high water / snow / ice
    /// 2fc7                 if (right flag == 1) { clear it; col = x + 6; vx = +3; dir = 0; facing = 0 }
    /// 2ff4                 else if (left flag == 1) { clear it; col = x; vx = -3; dir = 6; facing = 1 }
    /// 2ff4                 else return
    /// 301b                 0x32F7 = 0; 0x32EF = y - 0x12
    /// 302c                 0x32CB = 2; call 0x277c                   ; the eight-frame throw
    /// 3035                 0x2F34 = 0                              ; the tap counter 2d7e needs
    /// ```
    ///
    /// Two things are worth stating because they are not what a guess would produce: the throw
    /// costs **nothing** — no snow, no item, no heat, no clock — and a **westward** throw starts on
    /// the spy's own column while an eastward one starts six bytes ahead, so a spy standing at the
    /// thrower's own column can never be hit. `0x32F7` is written here and read nowhere: dead.
    ///
    /// The direction the throw writes into `0x2F0C` (0 east, 6 west) is wiped by `277c` itself
    /// (`CS:27f1`, the `state < 0x1E` path) — the recorded throws all show dir 0, and only the
    /// facing tells east from west.
    fn throw_snowball(&mut self, i: usize, dir_edge: [bool; 4]) -> bool {
        if self.players < 2 {
            return false;
        }
        let o = 1 - i;
        // The two visitor flags are only ever set for spies on the *same* screen (`FUN_26c0_0000`),
        // so testing the screen as well is the same rule and keeps a one-player round — whose
        // absent spy carries `blank` — from throwing at nobody.
        if self.spies[i].screen != self.spies[o].screen
            || !(self.spies[0].blank || self.spies[1].blank)
        {
            return false;
        }
        if self.spies[o].frozen >= 1 || self.spies[o].out_of_round {
            return false;
        }
        if self.spies[i].ball.map(|b| b.flying) == Some(true) {
            return false;
        }
        {
            let (screen, x, y) = {
                let s = &self.spies[i];
                (s.screen, s.x(), s.y())
            };
            if let Some(code) = self.terrain.code(&self.levels, screen, x, y) {
                if BALL_REFUSED_TERRAIN.contains(&code) {
                    return false;
                }
            }
        }
        // `CS:2fc7` is tested before `CS:2ff4`: a right press wins a frame that somehow holds both.
        let (mirror, lead, vx) = if dir_edge[3] {
            (false, BALL_EAST_LEAD, BALL_SPEED)
        } else if dir_edge[2] {
            (true, 0, -BALL_SPEED)
        } else {
            return false;
        };
        let (x, y) = (self.spies[i].x(), self.spies[i].y());
        self.spies[i].mirror = mirror;
        self.spies[i].ball = Some(Ball {
            row: y - BALL_ROW_FROM_FEET,
            col: x + lead,
            vx,
            flying: false,
        });
        self.spies[i].act_state = round::STATE_THROW;
        self.start_anim(i, round::STATE_THROW, None);
        self.spies[i].fire_taps = 0; // `CS:3035` / `CS:2ddc`
        // `CS:30C9..30DC`: the throw's noise — 24 one-tick notes, 104 ms.
        self.cue(sound::cue::SNOWBALL);
        true
    }

    /// `CS:303c` — **one frame of a snowball's flight**, run once per frame per thrower from the
    /// frame body's *draw* pass (`CS:e6a`): after both spies' frame bodies, which is why a hit
    /// leaves the victim's animation on its seed frame `-2` until the next frame.
    ///
    /// ```text
    /// 303e  bx = 0x32EF (row); cx = 0x32F3 (col) + 0x32FB (vx); 0x32F3 = cx
    /// 304e  if (row > 0x35) -> end
    /// 3056  ax = cx + 2; di = me ^ (0x2F7E ? 2 : 0); ax -= mapX(di)
    /// 306c  if (ax < 0) -> end                                   ; off the left of the window
    /// 3071  if (ax - 2 - 0x2C >= 0) -> end                        ; off the right
    /// 307c  the hit test against the OTHER spy (see `snowball_hits`)
    /// 3130  (no hit) draw the record at DS:0x6F66 at (col - mapX, row)
    /// 3155  (end) 0x2F40 = 0
    /// ```
    fn snowball_step(&mut self, i: usize) {
        let Some(mut b) = self.spies[i].ball else { return };
        if !b.flying {
            return;
        }
        b.col += b.vx;
        let view = self.spies[self.host_slot(i)].map_x;
        let inside = b.row <= BALL_ROW_MAX
            && b.col + BALL_VIEW_MARGIN >= view
            && b.col - view < BALL_VIEW_WIDTH;
        if !inside {
            self.spies[i].ball = Some(Ball { flying: false, ..b });
            return;
        }
        let o = 1 - i;
        if self.snowball_hits(i, o, b) {
            self.spies[i].ball = Some(Ball { flying: false, ..b });
            self.snowball_knock_out(i, o);
        } else {
            self.spies[i].ball = Some(b);
        }
    }

    /// `CS:307c..30c6` — the hit test, as two inclusive boxes plus the victim's own three gates:
    ///
    /// ```text
    /// 3080  ax = his.x + 1; if (col < ax) miss; ax += 3; if (col > ax) miss   ; [x+1, x+4]
    /// 3095  ax = his.y - 0x18; if (row < ax) miss; ax += 0xA; if (row > ax) miss ; [y-24, y-14]
    /// 30aa  if (his.0x2F68 != 0) miss        ; a climbing spy ducks the ball
    /// 30b1  if (his.0x329F >= 0xA) miss      ; indoors there is no ball to hit him with
    /// 30b8  if (his.0x2F18 && his.0x32CF >= 0xC) miss  ; a slip, a fall, a laugh, a blast
    /// ```
    ///
    /// There is **no ownership test and no friendship test** — the ball is the other spy's by
    /// construction, and a spy who is *throwing* (state 2) can be hit precisely because 2 < 0x0C.
    ///
    /// **stage 21a** — and **no direction term**: the box is the victim's own x, never a mirrored
    /// one, so both directions are compared against the same `[his.x + 1, his.x + 4]`. What differs
    /// is only where the column starts (`BALL_EAST_LEAD`), and because the step precedes this test,
    /// an eastward throw at contact range is already five bytes past a nose-to-nose victim's box and
    /// can never come back. That is the original's behaviour, not this routine's gap: `playtest5`
    /// `ds_00660` (west, `dx = -4`, hit column 213 against a box `[213, 216]`) is the recording's
    /// only point-blank knock-out, and `ds_00820`/`ds_00821` is the closest eastward one at `+6`.
    fn snowball_hits(&self, _thrower: usize, o: usize, b: Ball) -> bool {
        let (hx, hy) = (self.spies[o].x(), self.spies[o].y());
        if !(b.col >= hx + BALL_HIT_X[0] && b.col <= hx + BALL_HIT_X[1]) {
            return false;
        }
        if !(b.row >= hy - BALL_HIT_Y[0] && b.row <= hy - BALL_HIT_Y[1]) {
            return false;
        }
        let s = &self.spies[o];
        if s.pose_clip != 0 {
            return false;
        }
        if s.screen >= 0xA {
            return false;
        }
        if s.pose_raised && s.pose_state >= round::STATE_LAUGH {
            return false;
        }
        true
    }

    /// `CS:30c9..312d` — **the knock-out**. `0x32CF = -1` is written first, and the animation state
    /// that follows is chosen from the victim's facing and the ball's velocity:
    ///
    /// ```text
    /// 30c9  0x32CF(him) = -1
    /// 30cf  sound 0x9F60 (cue) / -2 (id)
    /// 30e2  0x2F40(me) = 0                     ; the ball dies where it lands
    /// 30f4  ax = his.0x2F14 + my.0x32FB
    /// 30fd  ax ==  4 -> 0x16 ; ax == -2 -> 0x18
    /// 3113  ax ==  3 -> 0x18 ; ax == -3 -> 0x16
    /// 3126  0x32CB(him) = ax; call 0x277c
    /// ```
    ///
    /// So `0x16` is the ball meeting his **face** and `0x18` the ball taking him from **behind**.
    /// Nothing else moves: no item is put away (`315f` is *not* called), the position is untouched
    /// (unlike the icicle's `y = 0x26`) and no byte of the object blocks changes. What *does* move
    /// is his body heat: `CS:30ee` adds **4 cold** (`mov $0xFFFE,%ax`, two steps) before the facing
    /// sum is taken — stage 16b read that call as the sound id `-2` and left the damage out
    /// (stage 20a). The animation is two frames — a flinch, not a 3.6 s knock-out.
    fn snowball_knock_out(&mut self, thrower: usize, victim: usize) {
        let vx = self.spies[thrower].ball.map(|b| b.vx).unwrap_or(0);
        let sum = self.spies[victim].mirror as i32 + vx;
        // `0x32CF = -1` is the word the routine writes first; `277c` overwrites it with the state
        // when it accepts the animation, so the -1 only survives a refusal (a victim locked in a
        // state above 2), exactly as `CS:30c9`/`CS:27b6` leave it.
        self.spies[victim].pose_state = 0xFFFF;
        // `CS:30e9`..`30f1` — `SI` is switched to the **victim** and `mov $0xFFFE,%ax` adds two
        // steps of cold (4) to *his* word. It sits before the facing sum, so it is paid whatever
        // the animation turns out to be. Stage 16b's report carries this as the sound id `-2`.
        self.heat_step(victim, -DAMAGE_SNOWBALL_STEPS);
        let state = match sum {
            4 | -3 => round::STATE_KNOCK_FACE,
            3 | -2 => round::STATE_KNOCK_BACK,
            _ => return,
        };
        self.spies[victim].act_state = state;
        self.start_anim(victim, state, None);
    }

    /// `FUN_26c0_2a4e` — the trapulator's own input, when the panel is open (`CS:076b`).
    ///
    /// The routine, branch for branch (`CS:2a4e`..`2b7e`):
    ///
    /// ```text
    /// 2a56  if buttonA == 1:                       ; fire
    /// 2a5e      buttonA = 0
    /// 2a66      if (kit & word[0x2D8C+cursor]) == 0 -> buzz, return   ; the empty slot
    /// 2a82      if cursor == 0xC -> 0x2F48 = 1 (the map), close
    /// 2aa9      code = word[0x2D9A + 2*cursor + SI]; 0x2F2C = code; 0x2EF8 = pose[code]
    /// 2abf      if code == 8: --0x330F; if (!= 0) close and keep the bit
    /// 2acd      kit ^= word[0x2D8C+cursor]
    /// 2adc  else if (up + down) != 0 -> close the panel
    /// 2aed      else if left:  if (0x2F9E) --0x2F9E else { 0x2F9E = 2; cursor -= 2; if (cursor < 0) cursor = 0xC }
    /// 2b2c      else if right: same, cursor += 2; if (cursor > 0xC) cursor = 0
    /// 2b7e      store the cursor
    /// ```
    ///
    /// **The horizontal pair moves the cursor and the vertical pair closes the panel** — stage 7
    /// had it the other way round (it read the `left+right == 0` guard as "up/down" and the
    /// `0x328F`/`0x3293` pair as "north/south"; `0x328F` is left, because `FUN_26c0_32b4` writes
    /// `0x2F04 = -1, 0x2F14 = 1`, and `0x3287` is up, because `FUN_26c0_3326` writes
    /// `0x2F00 = -2`). That is what the owner saw as "left/right does not select".
    ///
    /// The cursor's repeat delay `DS:0x2F9E` is the original's: it moves at once on the first
    /// frame the key is seen and then every third frame while it is held. The counter is *not*
    /// reset on release, so a quick re-press can cost one or two frames before it moves again —
    /// kept, because it is what the code does.
    fn trapulator(&mut self, i: usize, keys: Keys, fire_edge: bool) {
        let img = self.exe[DS_FILE_OFF..].to_vec();
        let cursor = self.spies[i].trap_cursor;
        if fire_edge {
            let bit = items::slot_bit(&img, cursor);
            if self.spies[i].kit & bit == 0 {
                // 0x2a6c: the empty slot — the code plays its buzz (`0xA51E`) and nothing else.
                self.cue(sound::cue::PANEL_DENIED);
                return;
            }
            if cursor == items::SLOT_MAP {
                // `0x2a87`: the map slot sets `DS:0x2F48+si` and jumps to the panel's own close
                // (`0x2b6e`), so **the panel goes down and the map comes up** — the original never
                // keeps the trapulator open through the look. `0x2f48` is written as the constant
                // 1, and its sign is the blink phase from then on (`CS:2CD5`).
                self.spies[i].trap_open = false;
                self.spies[i].fire_taps = 0;
                self.spies[i].map_open = true;
                self.spies[i].map_sign = 1;
                self.spies[i].cursor_req = 0;
                return;
            }
            let code = items::slot_code(&img, cursor, i);
            // `CS:2A90..2A9D`: the panel's A on a held slot — but 0x2a82's `cmp $0xc` sends the map
            // slot to 0x2a87 *before* this, so the map is the one selection with no cue.
            self.cue(sound::cue::SELECT);
            self.set_carried(i, code);
            self.set_pose(i, code);
            if code == items::CODE_DYNAMITE {
                self.spies[i].dyn_sticks = self.spies[i].dyn_sticks.saturating_sub(1);
                if self.spies[i].dyn_sticks != 0 {
                    self.trap_close(i);
                    return;
                }
            }
            self.spies[i].kit ^= bit;
            self.item_counts[5] += 1;
            self.trap_close(i);
            return;
        }
        // 0x2adc: up or down closes the panel.
        if keys.up || keys.down {
            self.trap_close(i);
            return;
        }
        // 0x2aed / 0x2b2c: left and right walk the cursor along the strip, with the repeat delay.
        // The *request* (`cursor_req`) is what decides when the keys themselves are not down this
        // frame: a press that arrived during the delay is still served, once, when the delay runs
        // out — the original returns at `CS:2afd`/`2b3c` without clearing the flag.
        let req = self.spies[i].cursor_req;
        let step: i32 = if keys.left || req < 0 {
            -2
        } else if keys.right || req > 0 {
            2
        } else {
            0
        };
        if step == 0 {
            return;
        }
        if self.spies[i].cursor_delay != 0 {
            self.spies[i].cursor_delay -= 1;
            return;
        }
        self.spies[i].cursor_delay = 2;
        // `CS:2b0a` / `CS:2b49`: the flag is cleared on the frame that moves, and the handler sets it
        // again on the next frame the key is held — so a held key repeats every third frame, and a
        // press that was only *latched* (the key is already up by now) is served exactly once.
        self.spies[i].cursor_req = 0;
        let mut next = cursor as i32 + step;
        if next < 0 {
            next = items::SLOT_MAP as i32;
        } else if next > items::SLOT_MAP as i32 {
            next = 0;
        }
        self.spies[i].trap_cursor = next as u16;
        // `CS:2B0E..2B1B` / `CS:2B4D..2B5A`: the cursor's click, one slide at div 4033 (296 Hz).
        self.cue(sound::cue::CURSOR);
    }

    fn trap_close(&mut self, i: usize) {
        self.spies[i].trap_open = false;
        self.spies[i].fire_taps = 0;
        // a request that was never served does not survive the panel: the flags in the original are
        // only meaningful while the panel is up, and a stale one would step the cursor on the next
        // open for no visible reason.
        self.spies[i].cursor_req = 0;
    }

    /// The action key and the mask's igloo **exit** (`FUN_26c0_2de3` reads no door at all; the
    /// exit is `FUN_26c0_1e0f`'s `AX == 8`). Returns `true` when the spy was moved, and the rest of
    /// the frame is skipped.
    ///
    /// The **entry** is not here any more: it is the walk-in path in [`Game::move_once`], which is
    /// where `FUN_26c0_1e0f`'s `AX == 7` branch lives in the original. `RoundOpts::igloo_entry` =
    /// [`IglooEntry::Key`] restores stage 4's action-key version for comparison — it fires at `cx`
    /// 21..22, `y` 40..44, i.e. one frame before the walk-in would.
    ///
    /// Out is the mask's code 8 (`FUN_26c0_1e0f`'s `AX == 8`): `screen -= 10`, `x = colbase + 0x16`,
    /// `y = 0x2A`. Every recorded exit was taken from `cx` 16, one step short of that cell — the
    /// cell fires inside the move, so the sample before it is the last one anyone sees.
    fn action(&mut self, i: usize, _keys: Keys, fire_edge: bool) -> bool {
        let (screen, x, y) = {
            let s = &self.spies[i];
            (s.screen, s.x(), s.y())
        };
        let col = self.terrain.col_of(x);
        let base = self.terrain.gate(col);
        let cx = x - base;

        if screen >= FIRST_INTERIOR_SCREEN {
            // the exit is the move path's `T_IGLOO_OUT` cell (see `Game::move_once`)
            return false;
        }

        if self.igloo_entry != IglooEntry::Key {
            return false;
        }
        // stage 4's reading: the key at the alcove in front of the code-7 cell
        let key_at_door = fire_edge
            && self.level_at(screen, col) == Some(LEVEL_DOOR_IGLOO)
            && (IGLOO_ENTER_CX.0..=IGLOO_ENTER_CX.1).contains(&cx)
            && (IGLOO_ENTER_Y.0..=IGLOO_ENTER_Y.1).contains(&y);
        if key_at_door && !self.igloo_occupied(i) {
            let s = &mut self.spies[i];
            s.screen = screen + 10;
            s.x3 = (base + 15) * SUB;
            s.y3 = 48 * SUB;
            s.map_x = (base - 8).clamp(0, MAP_X_MAX);
            s.frame = STAND_FRAME;
            s.frame_step = 0;
            self.after_room_step(i);
            return true;
        }
        false
    }

    fn door_at(&self, screen: u16, col: usize, cx: i32, levels: &[u16; 2]) -> bool {
        match self.level_at(screen, col) {
            Some(lv) => levels.contains(&lv) && (DOOR_CX_MIN..=DOOR_CX_MAX).contains(&cx),
            None => false,
        }
    }

    /// A **one-player** round has no second spy at all; its console is left in place and, by the
    /// visitor branch, flat cyan. The corpus contains no one-player round (the rig always drove
    /// two), so that step is `inferred` — the pixels are the ones measured for the visitor.
    fn blank_idle_console(&mut self) {
        if self.players < 2 {
            self.spies[1].screen = 0;
            self.spies[1].blank = true;
        }
    }

    // ---- the map (stage 18c) ---------------------------------------------------------------
    /// `DS:0x32FF` (+2) — the spy's **map column**: the gate column he is standing in, rounded down
    /// to even. `CS:3681` walks `0x25AE` down while `cx` is below the gate (the same search
    /// [`Terrain::col_of`] does), and `cx` there is the spy's own world x — `CS:36A0` subtracts
    /// `0x22` from `bx`, the *pixel* column, to get it. `CS:3689` then stores `(di >> 1) & 0xFFFE`,
    /// so the 12 gate columns become the map's six lattice columns, 0, 2, ... 10.
    ///
    /// **Not the scroll word**: on a clamped scroll (`playtest5` ds_00330: x = 127, `0x24A6` = 97)
    /// the two disagree and the recording follows the spy — `0x32FF` = 4 for x = 127, which is gate
    /// column 4, while the scroll's column is 2.
    pub fn map_col(&self, i: usize) -> u16 {
        (self.terrain.col_of(self.spies[i].x()) as u16) & !1
    }

    /// `row_base[band] + map_col` — the map cell the spy is standing in, and the word the trail
    /// holds. `CS:36C5`: `di = word[0x24AA + band]; di += word[0x32FF + si]`.
    ///
    /// `0x24AA` is read **at the band's own byte offset**, so band 2 is entry 1, band 6 entry 3.
    pub fn map_pos(&self, i: usize) -> u16 {
        let band = (self.spies[i].screen / 2) as usize;
        if band >= 8 {
            return self.map_col(i);
        }
        let a = round::DS_ROW_BASE + 2 * band;
        let base = u16::from_le_bytes([self.ds[a], self.ds[a + 1]]);
        base.wrapping_add(self.map_col(i))
    }

    /// `CS:36D1` — push the spy's cell into his five-entry trail when it changes.
    ///
    /// The original calls this from the move path and tests only "is it different from the newest
    /// entry", so calling it once a frame (after the frame's movement, before the draw) is the same
    /// thing: the trail is idempotent, and a cell he only passed through inside one frame is not
    /// recorded — exactly as in the original, which is why the trail is a coarse route.
    fn map_trail_step(&mut self, i: usize) {
        let pos = self.map_pos(i);
        if self.spies[i].trail[0] == pos {
            return;
        }
        for k in (1..5).rev() {
            self.spies[i].trail[k] = self.spies[i].trail[k - 1];
        }
        self.spies[i].trail[0] = pos;
    }

    /// `CS:2B85` / `CS:2B9E` — the two ways the map takes **itself** down. Both run inside the
    /// draw routine, so they take effect on the frame that would have drawn it:
    ///
    /// ```text
    /// 2b85  if (0x2f64[other] == 0 && 0x2f7e + 0x2f80 != 0) -> 0x2f48 = 0   ; the spies share a room
    /// 2b9e  if (0x2f68[si] > 6)                             -> 0x2f48 = 0   ; climbing a hatch
    /// ```
    ///
    /// The first rule is "the other spy is alive and one of the two consoles is a visitor's", i.e.
    /// both spies on one screen, where the map would give away the room. It is **gated on the port
    /// actually having two players**: a one-player port round blanks the second console for a spy
    /// the original always has, and no recorded one-player frame has either blank word set
    /// (`playtest5` ds_00195: `0x2F7E = 0x2F80 = 0`, map up). Code-read otherwise; the recorded
    /// map frames all have the spies apart (STAGE18C_REPORT §4.3).
    fn map_hidden(&self, i: usize) -> bool {
        let other = 1 - i;
        let shared_room = self.players > 1
            && self.spies[other].frozen == 0
            && (self.spies[0].blank || self.spies[1].blank);
        shared_room || self.spies[i].pose_clip > 6
    }

    // ---- stage 23c: the Big Storm ------------------------------------------------------------
    /// `CS:0B57..0x0B6A` — **is this console snowed on this frame?**
    ///
    /// ```text
    /// 0b57  cmpw $0x78,0x324d        ; the round clock, seconds — 120 is 2:00
    /// 0b5c  jg   0xba3               ; more than 2:00 -> no storm
    /// 0b5e  cmpw $0x0,0x2f7e(%si)    ; this spy is the VISITOR of the other's room?
    /// 0b63  jne  0xba3               ;   -> no storm (the flat console is never snowed)
    /// 0b65  cmpw $0x8,0x329f(%si)    ; screen id: 0, 2, 4, 6, 8 are outdoors
    /// 0b6a  jg   0xba3               ; indoors -> no storm
    /// ```
    ///
    /// It is a timer and nothing else: no island term, no difficulty term, no roll. From 2:00
    /// until the round ends, on every outdoors host console, every frame. The `no_host` case
    /// (both visitor flags set) is covered by the `blank` test, which is what the original's
    /// per-console `0x2F7E` test does too.
    pub fn storm_active(&self, i: usize) -> bool {
        !self.storm_off && self.clock_left <= 120 && !self.spies[i].blank && self.spies[i].screen <= 8
    }

    // ---- drawing ---------------------------------------------------------------------------
    /// The room viewport for one console, as 2bpp strip bytes (44 x 58). Exactly what
    /// `round.rs` composes for the room — plates, the per-cell object layer and the spy sprite —
    /// plus this port's debug marker when `--marker` is on.
    pub fn strip_for(&self, player: usize, show_marker: bool) -> [u8; STRIP_LEN] {
        let s = &self.spies[player];
        // **The no-host case.** A console is blank when its spy is the *visitor* of the other's
        // room — but when *both* flags are set there is no host at all, and the original still
        // composes each spy's own room and draws his statue in it: `freeze-death/f_014000.jpg`,
        // 30 s after both spies froze, shows both rooms with both figures still standing. Two
        // dumps of 19,395 in the corpus are in that state (ds_00036/37 of the same session), and
        // the freeze is the only way to reach it (STAGE14_REPORT §4.5).
        let no_host = self.players > 1 && self.spies[1 - player].blank;
        let mut strip = if s.blank && !no_host {
            // The blanking branch (`0x2F7E`, `CS:0x1A245`) resolves through the tables to plate
            // 0x46B0, which ships as a flat 0x55 plate — measured exact on 84 dumps. (Composing it
            // through `compose_passes` gives the same bytes; the flat fill just says so directly.)
            [0x55u8; STRIP_LEN]
        } else {
            // No special case for the igloo interiors (screen >= 10): the game does not have one
            // either. Its level row is read past the 5x12 table into the words that follow it, and
            // reading through is what reproduces those dumps — clamping or short-circuiting them to
            // a flat fill costs every byte of 247 of the 1082 recorded strips (stage 2A §9.4).
            let st = self.round_state();
            let art = match Art::from_exe(&self.exe) {
                Ok(a) => a,
                Err(_) => return [0u8; STRIP_LEN],
            };
            let (mut strip, _) = round::compose_passes(&art, &st, player);
            let ds_img = &st.ds[..0x10000.min(st.ds.len())];
            if self.objects {
                round::draw_objects(&mut strip, ds_img, &st, player);
            }
            if self.spies_layer {
                // `FUN_26c0_0c93` draws **both** spies into a console when the other spy's flag is
                // set, and the order is a y-sort, not a player order: `FUN_26c0_36f0` swaps the two
                // entries of the spy-pointer table until `table[0].y >= table[2].y`, and the draw
                // loop then walks `iVar1 = 2` and `iVar1 = 0` — so the spy with the **smaller y**
                // (further back) is drawn first and the nearer one lands on top. On a tie nothing
                // is swapped, so player 1 (table[0]) is the one drawn last. Both are positioned
                // against *this* console's `map_x`, which is why the host sees the visitor walking
                // about his room (defect B).
                let shared = self.visitor_draw
                    && self.players > 1
                    && self.spies[1 - player].blank
                    && self.spies[1 - player].screen == self.spies[player].screen;
                if shared {
                    let (first, second) = if self.spies[1].y() > self.spies[0].y() {
                        (0usize, 1usize)
                    } else {
                        (1usize, 0usize)
                    };
                    round::draw_spy_at(&mut strip, &self.exe, ds_img, &st, first, player);
                    round::draw_spy_at(&mut strip, &self.exe, ds_img, &st, second, player);
                } else if s.blank && no_host {
                    // the no-host case again: his own room, his own statue (`draw_spy` refuses a
                    // blank spy, `draw_spy_at` is the visitor path and does not test the flag)
                    round::draw_spy_at(&mut strip, &self.exe, ds_img, &st, player, player);
                } else {
                    round::draw_spy(&mut strip, &self.exe, ds_img, &st, player);
                }
            }
            // `CS:0cc5` — a spy with `0x2F70` set is not drawn at all: `0xc93` calls `0x20ee`
            // **instead**, which is the rocket leaving the hatch. It replaces the spy's own pass, so
            // it is drawn after the objects and after the (absent) spy layer.
            if self.spies_layer && self.spies[player].launched {
                self.draw_rocket(&mut strip, player, ds_img);
            }
            // **stage 16b** — the snowball. `CS:3130..3152` draws the record at `DS:0x6F66` at
            // `(0x32F3 - mapX, 0x32EF)` in the console that owns the room: a thrower who is the
            // *visitor* draws his ball in the host's window (`di = bp ^ 2` at `CS:3136`), which is
            // the same rule the spy sprite follows. The ball word itself gates it (`CS:e6a`).
            if self.spies_layer {
                for q in 0..self.players {
                    let Some(b) = self.spies[q].ball else { continue };
                    if !b.flying {
                        continue;
                    }
                    let owner = if self.spies[q].blank { 1 - q } else { q };
                    if owner != player {
                        continue;
                    }
                    if let Some(rec) = round::ball_record(ds_img) {
                        let x = b.col - self.spies[player].map_x;
                        round::masked_blit(&mut strip, &rec, x, b.row, false);
                    }
                }
            }
            // **stage 23c — the Big Storm** (`CS:0B6C..0x0BA2`). Six copies of **one** 410-byte
            // flake tile are OR'd into rows 0..55 of the console, the copy chosen by the sign of
            // the pre-flip phase word: `si = 0x5FEC + (4 if word >= 0 else 0)` per band, `cx =
            // 0x19A`, and `di` walks straight through the buffer without being reset. 410 bytes
            // is 9 rows + 14 bytes of a 44-byte strip, so each stamp lands 14 bytes further right
            // — the tile lands on a diagonal, not on a row-aligned grid.
            //
            // The mask is the image's own bytes, read at `DS:0x5FEC` (never a constant array): the
            // project's rule is that changing a byte in the image changes the output.
            // The last three rows (bytes 2460..2551) are the plate's bottom wall and never snowed.
            if self.storm_active(player) {
                let phase = if self.spies[player].storm_phase >= 0 { 4usize } else { 0 };
                for band in 0..6usize {
                    for i in 0..0x19A {
                        strip[band * 0x19A + i] |= self.exe[DS_FILE_OFF + 0x5FEC + phase + i];
                    }
                }
            }
            // **stage 18c — the map.** `CS:0BA3`..`0BAD`, the last thing the per-spy draw loop does
            // before the work buffer is blitted to the screen: `if (word[0x2F48+si] != 0) call
            // 0x2b7f`, with `si` the console's own spy — so the window lands in his own viewport,
            // over everything the scene drew.
            if self.spies[player].map_open {
                round::draw_map(&mut strip, ds_img, player);
            }
            strip
        };
        if show_marker && !s.blank {
            // every spy standing in this console's room gets a marker, positioned against this
            // console's scroll (which is the host's when the room is shared)
            for q in 0..self.players {
                if self.spies[q].screen == s.screen {
                    self.draw_marker(&mut strip, q, player);
                }
            }
        }
        strip
    }

    /// **The rocket** (`FUN_26c0_20ee`, `CS:2156`) — the ascent, drawn in place of the spy.
    ///
    /// ```text
    /// 2102  cx = 2*byte[0x320F] - [0x24A6] + 2      ; the hatch's column, on screen     (211c)
    /// 2121  ax = 0x9E - 0x2EBC                      ; the body's row shift
    /// 2128  ax < 0 ? (0x31B2 = 0, bottom = 0x36 + ax) : (0x31B2 = ax, bottom = 0x36)
    /// 2139  si = 0x61FA, 44-byte pitch              ; the record, 160 rows x 9 bytes
    /// 2148  if (0x2EBC >= 0x96) <the flame>
    /// 2163  cx = 2*[0x320F] - [0x24A6] + 4          ; THE FLAME'S COLUMN — body + 2 bytes (217b)
    /// 2180  ax = 0xBA - 0x2EBC; si = word[0x2D34 + (0x2EBC & 2)]
    /// ```
    ///
    /// So the nose appears at the hatch and the body is *below* the room: the shift drops 2 a frame
    /// and the visible window walks down the record until the room is full, then the bottom starts
    /// rising (`0x9E`), and past `0xD2` nothing is drawn at all.
    ///
    /// **The flame's column is the body's `+2`, not `+4`** (stage 19a). The two `add`s at `CS:211c`
    /// and `CS:217b` are the *same* expression with `2` and `4` added to it — the record is 9 bytes
    /// (36 px) and the flame 5 (20 px), so `+2` centres the flame under the body. Stage 15 read the
    /// `+4` as an offset *from the body's already-adjusted x* and wrote `x + 4`, which put the flame
    /// 2 bytes (8 px) right of centre — the owner's "offset by half the width of the rocket".
    fn draw_rocket(&self, strip: &mut [u8; STRIP_LEN], player: usize, ds_img: &[u8]) {
        let asc = self.rocket_asc;
        let view = self.spies[player].map_x;
        let x = self.rocket_x - view + 2;
        if asc <= round::ASCENT_HIDE {
            if let Some(rec) = round::rocket_record(ds_img) {
                let (bottom, shift) = round::ascent_place(asc, round::ROCKET_SHIFT_BASE);
                round::masked_blit_clipped(strip, &rec, x, bottom, false, shift);
            }
        }
        if (round::ASCENT_FLAME..=round::ASCENT_FLAME_END).contains(&asc) {
            if let Some(rec) = round::flame_record(ds_img, asc) {
                let (bottom, shift) = round::ascent_place(asc, round::FLAME_SHIFT_BASE);
                round::masked_blit_clipped(strip, &rec, x + 2, bottom, false, shift);
            }
        }
    }

    /// **debug overlay** — an 8-px wide bar where the spy stands, in magenta/black. The real
    /// sprite layer (`round::draw_spy`) is what the engine draws now, so this is off by default
    /// (`--marker` / `F1` in the window turns it on) and exists only to make the spy's position
    /// legible in a screenshot nobody can zoom.
    fn draw_marker(&self, strip: &mut [u8; STRIP_LEN], spy: usize, console: usize) {
        let s = &self.spies[spy];
        let camera = self.spies[console].map_x;
        let sx = s.x() - camera;
        let sy = s.y();
        if sx < 0 || sx >= STRIP_PITCH as i32 {
            return; // not in this window
        }
        // White would vanish against the snowfields, so the bar is magenta/black (P1) or
        // black/magenta (P2): 8 px wide x 24 rows, both colours, legible on white and on cyan.
        let (a, b) = if spy == 0 {
            (0b10_10_10_10u8, 0b00_00_00_00u8)
        } else {
            (0b00_00_00_00u8, 0b10_10_10_10u8)
        };
        let x0 = sx.clamp(0, STRIP_PITCH as i32 - 2) as usize;
        for r in (sy - 24).max(0)..sy {
            if r < 0 || r >= STRIP_ROWS as i32 {
                continue;
            }
            let base = r as usize * STRIP_PITCH;
            strip[base + x0] = a;
            strip[base + x0 + 1] = b;
        }
        // the facing nose: one row at the top of the bar, on the side he is looking at
        let nose_x = if s.mirror { x0.saturating_sub(1) } else { (x0 + 2).min(STRIP_PITCH - 1) };
        let r = (sy - 24).max(0) as usize;
        if r < STRIP_ROWS {
            strip[r * STRIP_PITCH + nose_x] = a;
        }
    }

    /// The 320x200 frame: the round's own console surround (`TRAP.PIC`) with **both consoles**
    /// drawn in, plus the countdown in the pod.
    ///
    /// Both consoles is what the real screen always shows — a console with no room on it is *flat
    /// cyan*, not absent: measured on `recon/round/sessions/m3-heat`, where 539 of 855 frames have a
    /// single uniform value in the bottom viewport (`DS:0x2F80` = 1, player 2 the visitor in player
    /// 1's room) and every other pixel of the frame is unchanged. The engine marks such a console
    /// blank, so `strip_for` returns the flat plate for it.
    pub fn frame(&self, show_marker: bool) -> Screen {
        let mut screen = Screen {
            // **the round plate**: `decode(TRAP.PIC)` with both viewport rects filled flat cyan
            // (`crate::chrome`, `CS:0x29F3`). There is no captured copy of it in this port any more
            // (stage 29): a checkout without the player's `TRAP.PIC` starts from an empty screen
            // and still draws the two viewports and the four widgets.
            idx: match self.assets.plate.as_ref() {
                Some(p) if p.len() >= crate::PIXELS => p[..crate::PIXELS].to_vec(),
                _ => vec![0u8; crate::PIXELS],
            },
        };
        let views = [round::VIEW_TOP, round::VIEW_BOTTOM];
        for i in 0..2usize {
            let strip = self.strip_for(i, show_marker);
            let idx = round::strip_to_indices(&strip);
            let (vx, vy) = views[i];
            for row in 0..STRIP_ROWS {
                for col in 0..176 {
                    let (x, y) = (vx + col, vy + row);
                    if x < crate::W && y < crate::H {
                        screen.idx[y * crate::W + x] = idx[row * 176 + col];
                    }
                }
            }
        }
        self.draw_clock(&mut screen);
        self.draw_heat_bars(&mut screen);
        if self.items_ui {
            self.draw_trapulator(&mut screen);
            self.draw_kit_box(&mut screen);
            self.draw_cursor(&mut screen);
        }
        screen
    }

    /// Draw into the screen's **byte image** — the 80-bytes-a-row 2bpp form every chrome routine
    /// addresses (`di = row_table[bx] + cx` in the original). `crate::Screen` holds one palette
    /// index per pixel, so the frame is packed, the widgets are drawn into it byte-addressed, and
    /// it is unpacked again; the round trip is exact.
    fn with_chrome_bytes<F: FnOnce(&mut [u8])>(&self, screen: &mut Screen, f: F) {
        let mut bytes = chrome::pack(&screen.idx);
        f(&mut bytes);
        screen.idx = chrome::unpack(&bytes);
    }

    /// The trapulator's dot strip: one record per tool, cyan core when the tool is in the
    /// trapulator, black when it is not.
    ///
    /// `FUN_26c0_3278` — `crate::chrome::draw_tray_dots`: seven **opaque 4-row records** out of
    /// `DS:0x15BC` (held) / `DS:0x15CA` (absent), indexed by the code's own `bp` = 0x0C..0, at the
    /// byte columns `DS:0x2E0E` holds (58, 53, 47, 42, 36, 31, 25 -> x 232..100) and the row
    /// `DS:0x2E0A` holds (74 / 182). The records encode the whole little box, outline included, so
    /// they are stamped and not merged; the 2x2 core the ROI reader scores is the record's own
    /// rows 1..2 at +3 px.
    ///
    /// A data segment too short for the records draws no dots at all.
    fn draw_trapulator(&self, screen: &mut Screen) {
        let ds = &self.ds;
        if ds.len() >= 0x10000 {
            for i in 0..2usize {
                if i >= self.players {
                    continue;
                }
                let kit = self.spies[i].kit;
                self.with_chrome_bytes(screen, |bytes| {
                    chrome::draw_tray_dots(bytes, ds, i, kit);
                });
            }
            return;
        }
        let img = &self.exe[DS_FILE_OFF..];
        for i in 0..2usize {
            if i >= self.players {
                continue;
            }
            for slot in 0..7usize {
                let bit = items::slot_bit(img, (slot * 2) as u16);
                let on = self.spies[i].kit & bit != 0;
                let v = if on { items::DOT_HELD } else { items::DOT_ABSENT };
                for dy in 0..2usize {
                    for dx in 0..2usize {
                        let x = items::TRAY_DOT_X[slot] + dx;
                        let y = items::TRAY_DOT_Y[i] + dy;
                        if x < crate::W && y < crate::H {
                            screen.idx[y * crate::W + x] = v;
                        }
                    }
                }
            }
        }
    }

    /// The kit box's inputs, as `FUN_26c0_31c8` reads them out of the data segment.
    pub fn kit_box_state(&self, i: usize) -> chrome::KitState {
        chrome::KitState {
            kit: self.spies[i].kit,
            carried: self.spies[i].carried,
            blink: self.spies[i].kit_blink,
            owner: self.part_owner,
        }
    }

    /// The **kit box**: the three rocket-part cells at the left of each console's tray, which the
    /// original composes every frame in `FUN_26c0_31c8`.
    ///
    /// The box itself is a 10-row x 11-byte canvas of the record's own `FF` (`DS:0x1494`,
    /// `CS:0x31c9`'s `rep movsb`) stamped by the masked blitter with up to three 2-plane records and
    /// copied to `(row DS:0x249E+si, byte column 0x0A)` — x 40..83, rows 76..85 / 184..193. All of
    /// it is `crate::chrome::draw_kit_box`: the icons are the records at `DS:0x6E5C` (12x6, left),
    /// `DS:0x6EA4` (8x7, middle) and `DS:0x6E82` (8x8, right), and the two passes are
    ///
    /// * **carried** — `carried >= 9` and a positive blink phase: the part's kit bit is set, so it
    ///   is in the case and the whole box blinks at the phase's rate;
    /// * **deposited** — this spy has put that part down (`DS:0x322F+bp == si`) and its kit bit is
    ///   clear: drawn solid, which is the owner's "a found rocket part does not light in the kit
    ///   display".
    ///
    /// Cell order is the code's `bp` = 0x16, 0x14, 0x12: **left = code 11 (punch card), middle =
    /// code 10 (fuel canister), right = code 9 (gyroscope)**. The icons are stamped from the
    /// records in `spy.exe` itself (`chrome::kit_icons`); the port keeps no copy of them.
    fn draw_kit_box(&self, screen: &mut Screen) {
        if self.ds.len() >= 0x10000 {
            for i in 0..self.players.min(2) {
                let st = self.kit_box_state(i);
                self.with_chrome_bytes(screen, |bytes| {
                    chrome::draw_kit_box(bytes, &self.ds, i, &st);
                });
            }
            return;
        }
        // A data segment too short for the records draws no box at all; the icons live in
        // `spy.exe` and this port keeps no copy of them (stage 29).
    }

    /// Which of the kit box's three cells are lit for player `i`, in cell order (left, middle,
    /// right = codes 11, 10, 9).
    ///
    /// The two branches of `FUN_26c0_31c8` and nothing else, so the drawing and the `--state`
    /// report cannot drift apart:
    ///
    /// * the *found-in-the-snow* cell lights when the part is this spy's **and** its kit bit is
    ///   clear (it has been dropped again since it was found);
    /// * the *in-the-briefcase* cell lights when the spy is carrying a part or the case, the kit
    ///   bit is set, and the box's blink phase is non-negative — so it flashes at 10 Hz.
    pub fn kit_cells(&self, i: usize) -> [bool; 3] {
        let img = &self.exe[DS_FILE_OFF..];
        let s = &self.spies[i];
        let flashing = s.carried != CARRY_NONE && s.carried >= 9 && s.kit_blink >= 0;
        let mut out = [false; 3];
        for (cell, &code) in items::KIT_CELL_CODES.iter().enumerate() {
            let bit = items::code_bit(img, code);
            let owned = items::part_owner_slot(code)
                .map(|slot| self.part_owner[slot] == 2 * i as u16)
                .unwrap_or(false);
            out[cell] = (owned && s.kit & bit == 0) || (flashing && s.kit & bit != 0);
        }
        out
    }

    /// The trapulator's selection mark: `FUN_26c0_2cda`, called only while the panel is open.
    ///
    /// Two sprites per slot (`DS:0x15BC` for one blink phase, `DS:0x15CA` for the other) blitted at
    /// the slot's own cell: x = 4 * `DS:0x2E0E[cursor]` (100, 124, 144, 168, 188, 212, 232) and
    /// row 74 (top console) / 182 (bottom). The two variants differ in four pixels, so the mark
    /// shimmers at 10 Hz — the original's "flashing" selection. **Read from the executable**
    /// (no recorded frame has the panel open, so this is not frame-verified: see the report).
    fn draw_cursor(&self, screen: &mut Screen) {
        let img = &self.exe[DS_FILE_OFF..];
        for i in 0..self.players.min(2) {
            let s = &self.spies[i];
            if !s.trap_open {
                continue;
            }
            let Some(mark) = items::cursor_mark(img, s.trap_cursor, i, s.trap_sign < 0) else {
                continue;
            };
            for dy in 0..mark.h {
                for dx in 0..mark.w * 4 {
                    if let Some(v) = mark.pixel(dx, dy) {
                        let (x, y) = (mark.x + dx, mark.y + dy);
                        if x < crate::W && y < crate::H {
                            screen.idx[y * crate::W + x] = v;
                        }
                    }
                }
            }
        }
    }

    /// The countdown, drawn into the console's own timer pod with the **game's own digit art**.
    ///
    /// `CS:0x229E` — `crate::chrome::draw_clock`: four **opaque 8 x 2 glyph records** out of the
    /// ten at `DS:0x7004 + 18*d` (`[h=8][w=2]` + 16 single-plane bytes), at row 0x62 (98), byte
    /// columns 0x4B / 0x49 / 0x45 / 0x43 — px 300 (second units), 292 (second tens), 276 (minute
    /// units) and 268 (minute tens). Each cell is written whole, background included; row 0 is
    /// uniformly the pod's magenta, which is why the seven-row glyph file this port used to carry
    /// (up to stage 29) was display-complete.
    ///
    /// A data segment that cannot supply the records falls back to this port's own 5x7 font —
    /// the only glyphs here that are the port's and not the game's.
    fn draw_clock(&self, screen: &mut Screen) {
        const DIGIT_ROWS: usize = 99;
        const DIGIT_X: [usize; 4] = [268, 276, 292, 300];
        let total = self.clock_secs().min(99 * 60 + 59);
        if self.ds.len() >= 0x10000 {
            let mut drew = false;
            self.with_chrome_bytes(screen, |bytes| {
                drew = chrome::draw_clock(bytes, &self.ds, total);
            });
            if drew {
                return;
            }
        }
        let digits = [(total / 60) / 10, (total / 60) % 10, (total % 60) / 10, total % 10];
        // no records to read: the port's own 5x7 font, which is this port's, not the game's
        for (k, d) in digits.iter().enumerate() {
            draw_glyph(screen, DIGIT_X[k], DIGIT_ROWS, *d);
        }
    }

    /// **The body-heat bars** — the two 12 x 60 gauges the console casing frames, at x 56..67, rows
    /// 8..67 (top console) and 116..175 (bottom).
    ///
    /// `FUN_26c0_21bd` — `crate::chrome::draw_heat_gauge`: the routine keeps a 60-row x 3-byte
    /// bitmap per player (`DS:0x330B` -> `DS:0x16C0` / `DS:0x177C`), seeds it from the template
    /// `DS:0x160A` and, per heat step, stamps two rows of the cyan (`DS:0x15EE`) or the white
    /// (`DS:0x15FC`) 14-byte row record into it before copying the whole bitmap to the screen at
    /// `DS:0x2F5C` (8 / 116), byte column 0x0E. Settled, with `L = DS:0x2F58` (3 = full heat, 62 =
    /// empty):
    ///
    /// ```text
    /// row 0        = the template's cap (`a8 00 02`)
    /// rows 1..L-3  = the WHITE row, by parity        (drained)
    /// rows L-2..59 = the CYAN row, by parity         (heat remaining)
    /// ```
    ///
    /// The parity is not a special case: the level moves by 2 and starts at 3, so the stamp's bottom
    /// row is always odd and record row 0 lands on the odd bitmap rows. Both rows put magenta at
    /// x 56/57 and black at x 67, which is why only x 58..66 ever changes.
    fn draw_heat_bars(&self, screen: &mut Screen) {
        if self.ds.len() >= 0x10000 {
            let heat = [self.spies[0].heat as i16, self.spies[1].heat as i16];
            self.with_chrome_bytes(screen, |bytes| {
                chrome::draw_heat_gauge(bytes, &self.ds, 0, heat[0]);
                chrome::draw_heat_gauge(bytes, &self.ds, 1, heat[1]);
            });
            return;
        }
        // A data segment too short for the row records draws no gauge: the bar is the records'
        // own bitmap (`chrome::heat_bitmap`), and this port keeps no copy of it (stage 29).
    }

    /// A `RoundState` for the *current* position: the static data segment with this round's level
    /// table and both spies' variables written into the slots the original uses.
    ///
    /// The pose block matters as much as the position: `DS:0x2F0C` (direction), `0x2EF8` (pose set),
    /// `0x2EFC` (walk phase), `0x32AB` (extra) and `0x2F14` (mirror) are what
    /// `round::draw_spy` indexes the record tables with. Writing `0x24` into `0x2F0C` — the pose
    /// *set* — is what made the sprite layer select `word[0x2622 + 0x24]` and draw nothing.
    pub fn round_state(&self) -> RoundState {
        let mut ds = self.ds.clone();
        for (r, row) in self.levels.iter().enumerate() {
            for (c, v) in row.iter().enumerate() {
                let off = round::DS_LEVELS + 24 * r + 2 * c;
                ds[off..off + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        for (i, s) in self.spies.iter().enumerate() {
            let o = 2 * i;
            let put = |ds: &mut Vec<u8>, addr: usize, v: u16| {
                ds[addr + o..addr + o + 2].copy_from_slice(&v.to_le_bytes());
            };
            put(&mut ds, round::DS_MAP_X, s.map_x as u16);
            put(&mut ds, round::DS_BAND, s.screen);
            put(&mut ds, round::DS_SPY_X, s.x().clamp(0, u16::MAX as i32) as u16);
            put(&mut ds, round::DS_SPY_Y, s.y().clamp(0, u16::MAX as i32) as u16);
            put(&mut ds, round::DS_BLANK, u16::from(s.blank));
            // `DS:0x2F64` — `>= 2` is "out of the round": `0xc93` skips the spy at `CS:cc0`. A
            // **launched** spy is not out of the round at all: the recording keeps his word at 0
            // on all 20 launch dumps (`playtest4` ds_01814, `rocket-room-2` ds_01309, ..) because
            // `0xc93` draws the rocket from `0x2F70` instead of him (`CS:cc5`). Writing 2 there was
            // this port's own invention and it hid the difference between "frozen out" and
            // "launched".
            put(&mut ds, round::DS_SPY_STATE, s.frozen.max(u16::from(s.out_of_round) * 2));
            // `DS:0x2F70` — the rocket, the word that takes this spy out of the player loop
            put(&mut ds, 0x2F70, u16::from(s.launched));
            // stage 14: the heat words and their two counters, so `--state`/`--from-ds` round-trip
            // and the bar can be read back out of the segment the original would see
            put(&mut ds, round::DS_HEAT, s.heat);
            put(&mut ds, round::DS_HEAT_T, s.terrain_t);
            put(&mut ds, round::DS_HEAT_BASE, s.base_t);
            round::write_pose(
                &mut ds,
                i,
                &round::PoseParams {
                    dir: s.dir,
                    set: s.set,
                    frame: s.frame,
                    extra: s.extra,
                    mirror: s.mirror,
                    row_shift: s.pose_row_shift,
                    raised: s.pose_raised,
                    state: s.pose_state,
                    clip: s.pose_clip,
                },
            );
            // **after** `write_pose`, which owns `0x2F2C` for the snowshoe row shift: the carried code
            // is the word itself (7 = snowshoes, which is the same fact from the other side), and
            // the kit word is what the trapulator's dots are drawn from.
            put(&mut ds, round::DS_CARRIED, s.carried);
            put(&mut ds, round::DS_KIT, s.kit);
            // stage 18c: the map's display word (with its sign — the blink phase), the spy's map
            // column and his five-entry trail, so `--state`/`--from-ds` round-trip the whole window
            put(&mut ds, round::DS_MAP_FLAG, if s.map_open { s.map_sign as u16 } else { 0 });
            put(&mut ds, round::DS_MAP_COL, self.map_col(i));
            for (k, v) in s.trail.iter().enumerate() {
                let a = round::DS_MAP_TRAIL + 10 * i + 2 * k;
                ds[a..a + 2].copy_from_slice(&v.to_le_bytes());
            }
            // the two blink phases and the cursor's repeat delay (stage 8)
            put(&mut ds, items::DS_KIT_BLINK, s.kit_blink as u16);
            put(&mut ds, items::DS_CURSOR_DELAY, s.cursor_delay);
            put(&mut ds, items::DS_COST_OPEN, if s.trap_open { s.trap_sign as u16 } else { 0 });
        }
        // **stage 18c** — the four position records the map draws dots from (`DS:0x3219` the
        // screen, `0x3205` the row, `0x320F` the column inside that row), written from the engine's
        // own record. `0xFFFF` in the screen word is "not placed" and the map skips it, which is
        // exactly what `CS:383F`/`CS:387F` write when a part is picked up.
        for (k, l) in self.links.iter().enumerate() {
            let a = round::DS_POS_RECORD + 2 * k;
            let screen = if l.used { l.screen } else { 0xFFFF };
            ds[a..a + 2].copy_from_slice(&screen.to_le_bytes());
            ds[0x3205 + 2 * k] = l.row;
            ds[round::DS_POS_COL + 2 * k] = l.col;
        }
        for (k, owner) in self.part_owner.iter().enumerate() {
            let a = items::DS_PART_OWNER + 2 * k;
            ds[a..a + 2].copy_from_slice(&owner.to_le_bytes());
        }
        // the hatch's own words: the armed flag (`0x31D5`), the bear's counter (`0x2F74`) and the
        // saved byte it replaces (`0x31B8`) — so a strip composed from the live state is the one the
        // original's own objects would draw
        ds[round::DS_HATCH_ARMED..round::DS_HATCH_ARMED + 2]
            .copy_from_slice(&self.hatch.armed.to_le_bytes());
        ds[round::DS_HATCH_SAVE] = self.hatch.saved;
        ds[round::DS_FIRES..round::DS_FIRES + 2].copy_from_slice(&self.fires.to_le_bytes());
        ds[0x2F74..0x2F76].copy_from_slice(&self.hatch.bear_frames.to_le_bytes());
        // stage 15: the ascent's counter (`0x2EBC`), the rocket's screen x (`0x2EBE`) and the
        // difficulty word (`0x31BF`), so a `--from-ds` run resumes a recorded ascent and a dump
        // round-trip carries the options screen's setting
        ds[0x2EBC..0x2EBE].copy_from_slice(&self.rocket_asc.to_le_bytes());
        let rx = (self.rocket_x - self.spies[0].map_x + 2).clamp(0, u16::MAX as i32) as u16;
        ds[0x2EBE..0x2EC0].copy_from_slice(&rx.to_le_bytes());
        ds[round::DS_DIFF..round::DS_DIFF + 2]
            .copy_from_slice(&(self.difficulty.saturating_mul(2)).to_le_bytes());
        RoundState::from_bytes(ds).expect("a 64 KB segment always parses")
    }

    // ---- verification: take a recorded dump's state and draw it through the engine ----------
    /// Load a recorded data-segment dump's state into the engine: both spies' positions, screens,
    /// scroll, direction, walk phase, pose set, facing and blank flag, plus the dump's own level
    /// table. This exists so the **engine's** compose path can be diffed against the strip the real
    /// game left in the same dump (`--from-ds` + `--check-engine`), which is the only way to prove
    /// the engine draws the original's room and not merely something plausible.
    ///
    /// `lift_blocks` copies the dump's five object blocks in. It is a verification aid, not game
    /// logic: the object/trap layer is built at round start by code this port does not model, so a
    /// live round starts with empty blocks (see `round::materialise_round_start`).
    pub fn seed_from_dump(&mut self, st: &RoundState, lift_blocks: bool) {
        self.levels = st.levels;
        // `DS:0x324D` holds the round's remaining seconds (measured: 0x167 = 359 s on a 360 s
        // island round a second in), so the countdown can be seeded too — otherwise the pod shows
        // this port's clock and no full-frame comparison could ever close.
        let secs = u16::from_le_bytes([st.ds[round::DS_CLOCK_SECS], st.ds[round::DS_CLOCK_SECS + 1]]);
        if secs > 0 {
            self.clock_left = secs;
            self.clock_word = 0;
            self.clock_secs_total = secs;
        }
        for i in 0..2 {
            // A one-player round has no second spy, so its console stays flat cyan and p2 is not
            // seeded. (In the recorded corpus this only happens for the *visitor* crate — m3-heat's
            // blanked console is player 2's, `DS:0x2F80` = 1, both spies in band 4 — which is seeded
            // from the flag like any other. No one-player round was ever recorded.)
            if i == 1 && self.players < 2 {
                self.spies[1].blank = true;
                continue;
            }
            let p = &st.players[i];
            let s = &mut self.spies[i];
            s.screen = p.band;
            s.map_x = p.map_x as i32;
            s.x3 = p.spy.x as i32 * SUB;
            s.y3 = p.spy.y as i32 * SUB;
            s.dir = p.spy.pose;
            s.mirror = p.spy.mirror;
            s.blank = p.blank;
            // `DS:0x2F50` — the storm's phase word, so a recorded storm frame draws the copy the
            // recording did (the value is post-flip: the dump shows the negation of what was drawn).
            s.storm_phase = u16::from_le_bytes([st.ds[0x2F50 + 2 * i], st.ds[0x2F51 + 2 * i]]) as i16;
            if s.storm_phase == 0 {
                s.storm_phase = 1;
            }
            let pose = round::read_pose(&st.ds, i);
            s.set = pose.set;
            s.extra = pose.extra;
            s.frame = pose.frame;
            s.pose_row_shift = pose.row_shift;
            s.pose_raised = pose.raised;
            s.pose_state = pose.state;
            s.pose_clip = pose.clip;
            // `0x32A7` (the set an animation saved) and `0x2F70`/`0x2F64` (the rocket, out of the
            // round) are words the dump carries; without them a resumed launch would draw a spy
            // the original draws as a rocket.
            s.saved_set = u16::from_le_bytes([st.ds[0x32A7 + 2 * i], st.ds[0x32A8 + 2 * i]]);
            s.saved_dir = u16::from_le_bytes([st.ds[0x32AF + 2 * i], st.ds[0x32B0 + 2 * i]]);
            s.launched = u16::from_le_bytes([st.ds[0x2F70 + 2 * i], st.ds[0x2F71 + 2 * i]]) != 0;
            s.frozen = u16::from_le_bytes([st.ds[0x2F64 + 2 * i], st.ds[0x2F65 + 2 * i]]);
            s.out_of_round = s.frozen >= 2;
            // stage 14: the heat word and its two counters, so a replay resumes the recorded rate
            // instead of starting a fresh bar
            s.heat = u16::from_le_bytes([st.ds[round::DS_HEAT + 2 * i], st.ds[round::DS_HEAT + 2 * i + 1]]);
            s.terrain_t = u16::from_le_bytes([st.ds[round::DS_HEAT_T + 2 * i], st.ds[round::DS_HEAT_T + 2 * i + 1]]);
            s.base_t = u16::from_le_bytes([st.ds[round::DS_HEAT_BASE + 2 * i], st.ds[round::DS_HEAT_BASE + 2 * i + 1]]);
            if s.launched {
                self.rocket = true;
                // the ascent resumes where the dump left it (`0x2EBC`), and the rocket's column is
                // the dump's own `0x2EBE` turned back into a world x
                self.rocket_asc = u16::from_le_bytes([st.ds[0x2EBC], st.ds[0x2EBD]]);
                self.rocket_x = s.x() - 2;
            }
            // the item words, so a `--from-ds` run and the stage-7 tests start from the dump's
            // own hand
            let w = |addr: usize| u16::from_le_bytes([st.ds[addr + 2 * i], st.ds[addr + 2 * i + 1]]);
            s.carried = w(items::DS_COST_CARRIED);
            s.kit = w(items::DS_COST_KIT);
            s.dyn_sticks = w(items::DS_COST_DYN);
            s.case_owned = w(items::DS_COST_CASE) != 0;
            s.trap_cursor = w(items::DS_COST_CURSOR);
            s.trap_open = w(items::DS_COST_OPEN) != 0;
            // the open word's *sign* is the cursor flash's phase, and 0x2F4C is the kit box's
            s.trap_sign = w(items::DS_COST_OPEN) as i16;
            if s.trap_sign == 0 {
                s.trap_sign = 1;
            }
            s.kit_blink = w(items::DS_KIT_BLINK) as i16;
            if s.kit_blink == 0 {
                s.kit_blink = 1;
            }
            s.cursor_delay = w(items::DS_CURSOR_DELAY);
            // **stage 18c** — the map. The dump holds `0x2F48` *after* `CS:2CD5` negated it, so the
            // value the draw used is its negation; that is the only asymmetry in the round trip.
            let flag = w(round::DS_MAP_FLAG) as i16;
            s.map_open = flag != 0;
            s.map_sign = if flag == 0 { 1 } else { -flag };
            for (k, v) in s.trail.iter_mut().enumerate() {
                let a = round::DS_MAP_TRAIL + 10 * i + 2 * k;
                *v = u16::from_le_bytes([st.ds[a], st.ds[a + 1]]);
            }
            // stage 16b: a ball resumes from its own row, column and velocity. The three words are
            // written at the throw (`CS:3021..3009`) and kept after the ball dies, so a mid-throw
            // dump holds them with `0x2F40` still clear — both cases have to come back.
            if w(0x2F40) != 0 || w(0x32CB) == round::STATE_THROW {
                s.ball = Some(Ball {
                    row: w(0x32EF) as i32,
                    col: w(0x32F3) as i32,
                    vx: w(0x32FB) as i16 as i32,
                    flying: w(0x2F40) != 0,
                });
            }
        }
        self.mask = u16::from_le_bytes([st.ds[items::DS_COST_MASK], st.ds[items::DS_COST_MASK + 1]]);
        // the options screen's difficulty is the round's own word (`CS:1066`, no copy at round start)
        self.difficulty = u16::from_le_bytes([st.ds[round::DS_DIFF], st.ds[round::DS_DIFF + 1]]) / 2;
        // The hatch's own words, so a `--from-ds` run resumes a recorded sequence instead of
        // starting one: the pointer (`0x31B6`), the armed word (`0x31D5`), the bear's saved byte
        // (`0x31B8`) and its counter (`0x2F74`) are all in the dump.
        let hat = u16::from_le_bytes([st.ds[round::DS_OBJ_HATCH_PTR], st.ds[round::DS_OBJ_HATCH_PTR + 1]]);
        self.hatch.addr = if hat > 1 { Some(hat as usize) } else { None };
        self.hatch.armed = u16::from_le_bytes([st.ds[round::DS_HATCH_ARMED], st.ds[round::DS_HATCH_ARMED + 1]]);
        self.hatch.saved = st.ds[round::DS_HATCH_SAVE];
        self.hatch.bear_frames = u16::from_le_bytes([st.ds[0x2F74], st.ds[0x2F75]]);
        self.hatch.bear = matches!(self.hatch.saved, 0) && self.hatch.bear_frames > 0;
        // Which spy is inside an animation is the pose block's own state word (`0x32CF`): a raised
        // spy in `0x1C`/`0x1E`/`0x20` is the hatch sequence, and a raised `0x0C` on the *other* spy
        // is the laugh that runs beside it. Frames come back from `0x2EFC` (`2f - 2`).
        self.hatch.seq = HatchSeq::Idle;
        self.hatch.other = None;
        for i in 0..2 {
            let pose = round::read_pose(&st.ds, i);
            if !pose.raised {
                continue;
            }
            let frames = ((pose.frame as i32 + 2) / 2).max(0) as u16;
            match pose.state {
                round::STATE_DESCEND => {
                    self.hatch.seq = HatchSeq::Descend;
                    self.hatch.spy = i;
                    self.hatch.frames = frames;
                }
                round::STATE_BEAR => {
                    self.hatch.seq = HatchSeq::Bear;
                    self.hatch.spy = i;
                    self.hatch.frames = frames;
                }
                round::STATE_CLOSE => {
                    self.hatch.seq = HatchSeq::Close;
                    self.hatch.spy = i;
                    self.hatch.frames = frames;
                }
                0x0C => {
                    self.hatch.other = Some(i);
                }
                // **stage 13a** — the tool and hazard animations are not the hatch's: they resume
                // as `Anim`, with the cell the saw and the bucket will write read back out of
                // `0x331F` (which is where `CS:2ea7`/`0x2f0b` put it when the action started).
                round::STATE_SAW | round::STATE_BUCKET => {
                    let cell = u16::from_le_bytes([st.ds[0x331F + 2 * i], st.ds[0x3320 + 2 * i]]);
                    self.spies[i].anim = Some(Anim {
                        state: pose.state,
                        frame: pose.frame as i16,
                        cell: (cell > 1).then_some(cell as usize),
                    });
                    self.spies[i].act_state = pose.state;
                }
                round::STATE_THROW | round::STATE_KNOCK_FACE | round::STATE_KNOCK_BACK
                | round::STATE_ICE_PICK | round::STATE_PLUNGER | round::STATE_SLIP
                | round::STATE_FALL | round::STATE_BLAST | round::STATE_KNOCKOUT
                | round::STATE_FREEZE => {
                    self.spies[i].anim = Some(Anim {
                        state: pose.state,
                        frame: pose.frame as i16,
                        cell: None,
                    });
                    self.spies[i].act_state = pose.state;
                }
                _ => {}
            }
        }
        for k in 0..items::PART_OWNERS {
            self.part_owner[k] =
                u16::from_le_bytes([st.ds[items::DS_PART_OWNER + 2 * k], st.ds[items::DS_PART_OWNER + 2 * k + 1]]);
        }
        // **stage 18c** — the four position records the map's dots are drawn from. The engine keeps
        // its own (`links`, maintained at round start, on every pickup, drop and stow); a seeded
        // frame has to take the *dump's*, or the dots would be the port's layout in the recording's
        // room. This is a verification aid, not game logic: a live round's records are the ones the
        // engine's own layout produced.
        for (k, l) in self.links.iter_mut().enumerate() {
            let a = round::DS_POS_RECORD + 2 * k;
            let screen = u16::from_le_bytes([st.ds[a], st.ds[a + 1]]);
            *l = cpu::Link {
                used: (screen as i16) >= 0,
                screen,
                row: st.ds[0x3205 + 2 * k],
                col: st.ds[round::DS_POS_COL + 2 * k],
            };
        }
        // **stage 18c** — the map plate. The dump carries the composed 88 x 45 bitmap at
        // `DS:0x1052`, which is the island's own blob word for word (STAGE18C_REPORT §2.1); the
        // engine takes the dump's copy so a seeded frame draws the island the *recording* is on,
        // whatever `--island` said.
        if st.ds.len() >= round::DS_MAP_PLATE + round::MAP_PLATE_LEN {
            self.ds[round::DS_MAP_PLATE..round::DS_MAP_PLATE + round::MAP_PLATE_LEN]
                .copy_from_slice(&st.ds[round::DS_MAP_PLATE..round::DS_MAP_PLATE + round::MAP_PLATE_LEN]);
        }
        if lift_blocks {
            for i in 0..5usize {
                let base = u16::from_le_bytes([
                    st.ds[round::DS_BLOCK_PTR + 2 * i],
                    st.ds[round::DS_BLOCK_PTR + 2 * i + 1],
                ]) as usize;
                if base + 624 <= self.ds.len() && base + 624 <= st.ds.len() {
                    self.ds[base..base + 624].copy_from_slice(&st.ds[base..base + 624]);
                }
            }
        }
        // **stage 19a** — a dump that resumes an ascent already carries the rocket's window picture
        // (`CS:2865..2877` ran on the launch frame), and re-applying the copy writes the same bytes:
        // the staged `[19][5]` records at `0x3C32`/`0x3C93` are static in every recorded dump. So
        // both paths — a resumed dump and a round the engine launches itself — end up drawing the
        // same window, and the seeded frame stops showing the redacted pink the template holds.
        for i in 0..2 {
            if self.spies[i].launched {
                self.stage_rocket_window(i);
            }
        }
    }

    /// The engine's own composition of one console's viewport, with whatever layers are enabled.
    pub fn engine_strip(&self, player: usize) -> [u8; STRIP_LEN] {
        self.strip_for(player, false)
    }

    /// The plate passes the engine's own state composes, for the verification report.
    pub fn engine_passes(&self, player: usize) -> Vec<round::Pass> {
        let st = self.round_state();
        match round::Art::from_exe(&self.exe) {
            Ok(art) => round::compose_passes(&art, &st, player).1,
            Err(_) => Vec::new(),
        }
    }

    // ---- reporting -------------------------------------------------------------------------
    /// The state JSON. `frame` must equal the number of steps applied (ADR 0002): for the engine a
    /// step is one tick, so this is `ticks`.
    pub fn state_json(&self) -> String {
        let s = &self.spies[0];
        // The hatch's own report, built separately: the words are `0x31D5` (armed), `0x31B6`
        // (address, -1 when the placement never found a cell), `0x32CF` (the state) and `0x2F74`.
        let hatch = format!(
            "{{\"armed\":{},\"addr\":{},\"seq\":\"{}\",\"spy\":{},\"frames\":{},\"bear_frames\":{},\"bear\":{},\"saved\":{},\"other\":{}}}",
            self.hatch.armed,
            self.hatch.addr.map(|a| a as i32).unwrap_or(-1),
            self.hatch.seq.name(),
            self.hatch.spy,
            self.hatch.frames,
            self.hatch.bear_frames,
            self.hatch.bear,
            self.hatch.saved,
            self.hatch.other.map(|o| o as i32).unwrap_or(-1)
        );
        format!(
            "{{\"frame\":{},\"scene\":\"round-play\",\"island\":{},\"players\":{},\"phase\":\"{}\",\
\"clock_secs\":{},\"clock_secs_total\":{},\"screen\":{},\"map_x\":{},\"x\":{},\"y\":{},\
\"dir\":{},\"walk_frame\":{},\"pose_set\":{},\"mirror\":{},\"indoors\":{},\"surround\":\"{}\",\
\"objects\":{},\"mask\":{},\"carried\":[{},{}],\"item_counts\":{:?},\
\"hatch\":{},\"fires\":{},\"rocket\":{},\"rocket_asc\":{},\"end\":\"{}\",\"difficulty\":{},\
\"hazards\":{},\"spies\":[{},{}]}}\n",
            self.ticks,
            self.data.island + 1,
            self.players,
            match self.phase {
                Phase::Playing => "playing",
                Phase::Over => "over",
            },
            self.clock_secs(),
            self.clock_secs_total,
            s.screen,
            s.map_x,
            s.x(),
            s.y(),
            s.dir,
            s.frame,
            s.set,
            s.mirror,
            self.in_interior(0),
            if self.assets.plate.is_some() { "derived" } else { "none" },
            self.layout.objects.len(),
            self.mask,
            if s.carried == CARRY_NONE { -1i32 } else { s.carried as i32 },
            if self.spies[1].carried == CARRY_NONE { -1i32 } else { self.spies[1].carried as i32 },
            self.item_counts,
            hatch,
            self.fires,
            self.rocket,
            self.rocket_asc,
            match self.end {
                Some(RoundEnd::Clock) => "clock",
                Some(RoundEnd::Launch) => "launch",
                Some(RoundEnd::BothOut) => "both-out",
                None => "none",
            },
            self.difficulty + 1,
            self.hazards_json(),
            self.spy_json(0),
            self.spy_json(1)
        )
    }

    fn spy_json(&self, i: usize) -> String {
        let s = &self.spies[i];
        format!(
            "{{\"x\":{},\"y\":{},\"screen\":{},\"map_x\":{},\"dir\":{},\"walk_frame\":{},\
\"walk_step\":{},\"pose_set\":{},\"mirror\":{},\"blank\":{},\"visitor\":{},\
\"carried\":{},\"carried_name\":\"{}\",\"kit\":{},\"dynamite\":{},\"briefcase_owned\":{},\
\"trap_open\":{},\"trap_cursor\":{},\"trap_slot\":\"{}\",\"action_state\":{},\
\"map_open\":{},\"map_sign\":{},\"map_cell\":{},\"map_col\":{},\"trail\":{:?},\
\"kit_blink\":{},\"kit_cells\":{},\"blocked\":{},\"raised\":{},\"pose_state\":{},\"clip\":{},\"launched\":{},\"out_of_round\":{},\
\"anim_state\":{},\"anim_frame\":{},\"anim_cell\":{},\"icicle_latch\":{},\"icicle_hit\":{},\
\"ball\":{{\"flying\":{},\"row\":{},\"col\":{},\"vx\":{}}},\
\"heat\":{},\"sink\":{},\"terrain_t\":{},\"base_t\":{},\"frozen\":{}}}",
            s.x(),
            s.y(),
            s.screen,
            s.map_x,
            s.dir,
            s.frame,
            s.frame_step,
            s.set,
            s.mirror,
            s.blank,
            s.blank,
            if s.carried == CARRY_NONE { -1i32 } else { s.carried as i32 },
            items::item_name(s.carried),
            s.kit,
            s.dyn_sticks,
            s.case_owned,
            s.trap_open,
            s.trap_cursor,
            items::SLOT_NAMES[(s.trap_cursor / 2).min(6) as usize],
            s.act_state,
            s.map_open,
            s.map_sign,
            self.map_pos(i),
            self.map_col(i),
            s.trail,
            s.kit_blink,
            format!(
                "[{},{},{}]",
                self.kit_cells(i)[0] as u8,
                self.kit_cells(i)[1] as u8,
                self.kit_cells(i)[2] as u8
            ),
            s.blocked,
            s.pose_raised,
            s.pose_state,
            s.pose_clip,
            s.launched,
            s.out_of_round,
            s.anim.map(|a| a.state as i32).unwrap_or(-1),
            s.anim.map(|a| a.frame as i32).unwrap_or(0),
            s.anim.and_then(|a| a.cell).map(|c| c as i32).unwrap_or(-1),
            s.icicle_latch,
            s.icicle_hit,
            s.ball.map(|b| b.flying).unwrap_or(false),
            s.ball.map(|b| b.row).unwrap_or(0),
            s.ball.map(|b| b.col).unwrap_or(0),
            s.ball.map(|b| b.vx).unwrap_or(0),
            s.heat,
            s.pose_clip,
            s.terrain_t,
            s.base_t,
            s.frozen
        )
    }

    /// **stage 13a** — every hazard byte in the five object blocks, as JSON, so a demo can assert
    /// on the world without parsing `--dump-objects`' text.
    pub fn hazards_json(&self) -> String {
        let mut out = Vec::new();
        for (b, base) in [0x53BCusize, 0x562C, 0x589C, 0x5B0C, 0x5D7C].iter().enumerate() {
            let lo = if b == 4 { 0x9C } else { 0 };
            for off in lo..0x270 {
                let byte = self.ds[base + off];
                if !matches!(byte, items::OBJ_SPILL | items::OBJ_HOLE | items::OBJ_ICICLE) {
                    continue;
                }
                let row = off / 0x9C;
                let entry = off % 0x9C;
                out.push(format!(
                    "{{\"band\":{},\"row\":{},\"entry\":{},\"code\":{},\"x\":{}}}",
                    2 * b, row, entry, byte, 2 * entry
                ));
            }
        }
        format!("[{}]", out.join(","))
    }
}

/// Does this screen id's row hold any door at all?
fn has_door(levels: [[u16; 12]; 10], screen: u16) -> bool {
    if screen % 2 != 0 || screen >= FIRST_INTERIOR_SCREEN {
        return false;
    }
    let row = (screen / 2) as usize;
    if row >= levels.len() {
        return false;
    }
    levels[row].iter().any(|&v| {
        LEVEL_DOOR_UP.contains(&v) || v == LEVEL_DOOR_DOWN || v == LEVEL_DOOR_IGLOO
    })
}

/// A 5x7 digit glyph for the clock pod (this port's own font, see `draw_clock`).
fn draw_glyph(screen: &mut Screen, x0: usize, y0: usize, d: u32) {
    const FONT: [[u8; 35]; 10] = [
        // 0
        [0, 1, 1, 1, 0, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1, 0],
        // 1
        [0, 0, 1, 0, 0, 0, 1, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 1, 1, 1, 0],
        // 2
        [0, 1, 1, 1, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 1, 1, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 1, 1, 1, 1],
        // 3
        [1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 1, 1, 1, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1, 0],
        // 4
        [0, 0, 1, 1, 0, 0, 1, 0, 1, 0, 1, 0, 0, 1, 0, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0],
        // 5
        [1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1, 0],
        // 6
        [0, 1, 1, 1, 0, 1, 0, 0, 0, 0, 1, 1, 1, 1, 0, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1, 0],
        // 7
        [1, 1, 1, 1, 1, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0],
        // 8
        [0, 1, 1, 1, 0, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1, 0, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1, 0],
        // 9
        [0, 1, 1, 1, 0, 1, 0, 0, 0, 1, 1, 0, 0, 0, 1, 0, 1, 1, 1, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 1, 1, 1, 0],
    ];
    let g = &FONT[(d % 10) as usize];
    for r in 0..7 {
        for c in 0..5 {
            if g[r * 5 + c] != 0 {
                let (x, y) = (x0 + c, y0 + r);
                if x < crate::W && y < crate::H {
                    screen.idx[y * crate::W + x] = 0;
                }
            }
        }
    }
}

// ---- scripts --------------------------------------------------------------
/// Parse a key script: one **tick** per line, tokens are the keys held during it, and a trailing
/// `*N` repeats the line. `#` starts a comment, `wait`/`none` are empty ticks.
///
/// ```text
/// # walk east to the door column, then step up through it
/// right *340
/// up *40
/// ```
pub fn parse_script(text: &str) -> Result<Vec<Input>, String> {
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut repeat = 1usize;
        let mut tokens: Vec<&str> = line.split_whitespace().collect();
        if let Some(last) = tokens.last() {
            if let Some(n) = last.strip_prefix('*') {
                repeat = n
                    .parse::<usize>()
                    .map_err(|_| format!("line {}: bad repeat {last:?}", lineno + 1))?;
                tokens.pop();
            }
        }
        let mut p1 = Keys::default();
        let mut p2 = Keys::default();
        for t in &tokens {
            // two-player scripts address a spy with a prefix: `1:up`, `2:q`
            let (who, key) = match t.split_once(':') {
                Some((a, b)) => (a, b),
                None => ("1", *t),
            };
            let slot = if who == "2" { &mut p2 } else { &mut p1 };
            match key.to_ascii_lowercase().as_str() {
                "up" | "n" => slot.up = true,
                "down" | "s" => slot.down = true,
                "left" | "w" => slot.left = true,
                "right" | "e" => slot.right = true,
                "fire" | "enter" | "return" | "q" => slot.fire = true,
                "wait" | "none" | "." => {}
                other => {
                    return Err(format!(
                        "line {}: unknown key {other:?} (up/down/left/right/fire/wait)",
                        lineno + 1
                    ))
                }
            }
        }
        // player 2's own key names, when the script does not prefix them
        if tokens.iter().any(|t| t.eq_ignore_ascii_case("a")) {
            p2.left = true;
        }
        if tokens.iter().any(|t| t.eq_ignore_ascii_case("x")) {
            p2.down = true;
        }
        if tokens.iter().any(|t| t.eq_ignore_ascii_case("d")) {
            p2.right = true;
        }
        for _ in 0..repeat {
            out.push(Input { p1, p2 });
        }
    }
    Ok(out)
}

/// Load and parse a script file.
pub fn load_script(path: &Path) -> Result<Vec<Input>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_script(&text)
}

// ---- tests ---------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn exe() -> Vec<u8> {
        std::fs::read(crate::orig::require("spy-vs-spy-iii/original/256/spy.exe")).expect("spy.exe")
    }

    fn game(opts: RoundOpts) -> Game {
        Game::new(exe(), &opts).expect("round")
    }

    /// One spy's pose parameters as the renderer reads them.
    fn pose_of(g: &Game, i: usize) -> round::PoseParams {
        let s = &g.spies[i];
        round::PoseParams {
            dir: s.dir,
            set: s.set,
            frame: s.frame,
            extra: s.extra,
            mirror: s.mirror,
            row_shift: s.pose_row_shift,
            raised: s.pose_raised,
            state: s.pose_state,
            clip: s.pose_clip,
        }
    }

    fn plain() -> Game {
        game(RoundOpts::default())
    }

    /// Hold one key for `n` ticks.
    fn hold(g: &mut Game, keys: Keys, n: usize) {
        for _ in 0..n {
            g.tick(Input { p1: keys, p2: Keys::default() });
        }
    }

    /// Hold one key until `done`, or `max` ticks have passed. Returns the ticks applied.
    fn until(g: &mut Game, keys: Keys, max: usize, done: impl Fn(&Game) -> bool) -> usize {
        for n in 0..max {
            if done(g) {
                return n;
            }
            g.tick(Input { p1: keys, p2: Keys::default() });
        }
        max
    }

    const UP: Keys = Keys { up: true, down: false, left: false, right: false, fire: false, fire_b: false };
    const DOWN: Keys = Keys { up: false, down: true, left: false, right: false, fire: false, fire_b: false };
    const LEFT: Keys = Keys { up: false, down: false, left: true, right: false, fire: false, fire_b: false };
    const RIGHT: Keys = Keys { up: false, down: false, left: false, right: true, fire: false, fire_b: false };
    const FIRE: Keys = Keys { up: false, down: false, left: false, right: false, fire: true, fire_b: false };

    #[test]
    fn island_table_is_the_original_data_segment() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // The whole per-island record: level table, clock, start screens, kit.
        let exe = exe();
        let clocks: Vec<u16> = (0..7)
            .map(|i| IslandData::from_exe(&exe, i).unwrap().clock_secs)
            .collect();
        assert_eq!(clocks, vec![360, 480, 660, 840, 1080, 1320, 1620]);
        for i in 0..7 {
            let d = IslandData::from_exe(&exe, i).unwrap();
            assert_eq!(d.start_screen_p1, 4, "island {}", i + 1);
            assert_eq!(d.start_screen_p2, 6, "island {}", i + 1);
        }
        assert_eq!(IslandData::from_exe(&exe, 0).unwrap().kit, 0x083A);
        assert_eq!(IslandData::from_exe(&exe, 2).unwrap().kit, 0x082A);
        assert_eq!(IslandData::from_exe(&exe, 6).unwrap().kit, 0x0820);

        // island 1: two rooms (screens 4 and 6) joined by a door pair at column 5
        let d = IslandData::from_exe(&exe, 0).unwrap();
        assert_eq!(d.levels[2], [8, 8, 6, 0, 0, 2, 16, 4, 8, 8, 8, 8]);
        assert_eq!(d.levels[3], [8, 8, 6, 16, 0, 10, 0, 4, 8, 8, 8, 8]);
        assert_eq!(d.levels[0], [8; 12]);
        assert_eq!(d.first_playable_row(), 2);
    }

    /// **Speed, both regimes.** A *clock second* of walking is **9 game frames** (stage 16a: the
    /// clock's word `DS:0x33F6` is tested once a frame, 9 x 26 ticks). While the viewport scrolls
    /// the original's loop takes two 1-unit steps a frame (the second iteration that the scroll
    /// asks for), so a clock second covers 18 units; with the view clamped at either end of the row
    /// there is no second iteration and the same second covers 9. (This test used to say "twenty
    /// units a second": that was 10 frames a second at 100 ms, the frame period stage 16a fixed.
    /// The wall-clock rate is 2 units / 108.34 ms = 18.5 units/s.)
    #[test]
    fn walk_is_eighteen_units_a_clock_second_while_the_view_scrolls() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // start in the middle of the room so the viewport is free to scroll
        let mut g = game(RoundOpts { start_x: Some(120), start_screen: Some(4), ..RoundOpts::default() });
        // walk into the dead-zone edge first: the second iteration only starts once the view
        // scrolls, which happens when the spy reaches x = map_x + 30
        hold(&mut g, RIGHT, (CLOCK_FRAMES * FRAME_TICKS) as usize);
        assert!(g.spies[0].x() - g.spies[0].map_x >= SCROLL_RIGHT_EDGE - 1,
                "the view is scrolling now: x={} map_x={}", g.spies[0].x(), g.spies[0].map_x);
        // one settling frame: the frame that first scrolls the view takes the short step
        hold(&mut g, RIGHT, FRAME_TICKS as usize);
        let x0 = g.spies[0].x();
        hold(&mut g, RIGHT, (CLOCK_FRAMES * FRAME_TICKS) as usize);
        assert_eq!(g.spies[0].x() - x0, WALK_UNITS_PER_SEC,
                   "9 frames at two units each is 18 units a clock second");
        assert_eq!(g.spies[0].dir, 0, "walking east is the horizontal pose");
        assert!(!g.spies[0].mirror);
        // back west: the first part of the return is inside the dead zone (one unit a frame),
        // the rest scrolls (two), so 20 frames return him past where he started
        hold(&mut g, LEFT, 20 * FRAME_TICKS as usize);
        assert!(g.spies[0].x() <= x0, "walked back west: {} vs {}", g.spies[0].x(), x0);
        assert!(g.spies[0].mirror);
        // the vertical directions set their own pose
        hold(&mut g, UP, FRAME_TICKS as usize);
        assert_eq!(g.spies[0].dir, 2);
        hold(&mut g, DOWN, FRAME_TICKS as usize);
        assert_eq!(g.spies[0].dir, 4);
    }

    /// At the ends of the world row the viewport cannot scroll, so the second iteration does not
    /// happen and the spy halves speed — 9 units a clock second (one a frame), which is what the
    /// original does there.
    #[test]
    fn at_the_scroll_clamp_the_same_loop_walks_at_half_speed() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // x = 10 puts the round-start scroll at its 0 clamp, with ten units of row to walk
        let mut g = game(RoundOpts { start_x: Some(10), start_screen: Some(4), ..RoundOpts::default() });
        assert_eq!(g.spies[0].map_x, 0, "the view is against its clamp");
        let x0 = g.spies[0].x();
        hold(&mut g, LEFT, (CLOCK_FRAMES * FRAME_TICKS) as usize);
        assert_eq!(x0 - g.spies[0].x(), WALK_UNITS_PER_SEC / 2,
                   "one unit a frame at the clamp, not two");
        assert_eq!(g.spies[0].map_x, 0);
    }

    // ---- stage 12: the walk in step (`CS:0743`) -------------------------------------------
    //
    // The owner's report: on a shared screen, walking the same way close together, the original
    // keeps the pair together and the port let the spy who was not pushing the view fall off at
    // half speed. Recording `orig-compare-1` has 15 such runs and every one holds a **constant**
    // separation; the rule behind it is `CS:0743` (see `Game::push_in_step`).

    /// Both spies on one screen, both walking one way: the pair keeps its spacing and both take the
    /// **same** step. The host stands at his west dead zone (`x - map_x == 6`), so his own move
    /// scrolls the view and he covers two units; the visitor covers two as well — one of his own and
    /// one shoved by the host's scroll.
    #[test]
    fn two_spies_walking_the_same_way_on_one_screen_keep_their_spacing() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_on_one_screen(136, 145, 130);
        let mut steps = Vec::new();
        for _ in 0..12 {
            let (a1, a2, m1) = (g.spies[0].x(), g.spies[1].x(), g.spies[0].map_x);
            for _ in 0..FRAME_TICKS {
                g.tick(Input { p1: LEFT, p2: LEFT });
            }
            steps.push((g.spies[0].x() - a1, g.spies[1].x() - a2, g.spies[0].map_x - m1));
        }
        assert!(g.spies[1].blank, "the east spy is the visitor on the host's console");
        assert!(!g.spies[0].blank, "and the west spy is the host");
        for (k, s) in steps.iter().enumerate().skip(1) {
            assert_eq!((s.0, s.1), (-2, -2),
                       "frame {k}: both spies step two units together (port had -1 for the \
                        visitor), got {s:?} over {steps:?}");
            assert_eq!(s.2, -2, "frame {k}: and the host's view scrolled two", );
        }
        assert_eq!(g.spies[1].x() - g.spies[0].x(), 9, "the separation never changed");
    }

    /// The same rule with the **visitor** leading: his own move is the one that scrolls the host's
    /// view, and the shove goes the other way — the host still keeps step.
    #[test]
    fn a_visitor_who_leads_still_keeps_step_with_the_host() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_on_one_screen(136, 129, 130);
        let mut steps = Vec::new();
        for _ in 0..12 {
            let (a1, a2) = (g.spies[0].x(), g.spies[1].x());
            for _ in 0..FRAME_TICKS {
                g.tick(Input { p1: LEFT, p2: LEFT });
            }
            steps.push((g.spies[0].x() - a1, g.spies[1].x() - a2));
        }
        assert!(g.spies[1].blank, "the west spy is the visitor here");
        for (k, s) in steps.iter().enumerate().skip(1) {
            assert_eq!(s, &(-2, -2), "frame {k}: visitor and host both take two units");
        }
        assert_eq!(g.spies[1].x() - g.spies[0].x(), -7, "the separation never changed");
    }

    /// The recorded `2/0` class is untouched: a spy who is **not** holding the key is not shoved
    /// (91 + 91 one-frame pairs in the corpus, e.g. `item-check-2` ds_00152).
    #[test]
    fn a_standing_spy_is_never_shoved() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_on_one_screen(136, 145, 130);
        for _ in 0..12 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input { p1: LEFT, p2: NOKEYS });
            }
        }
        assert_eq!(g.spies[1].x(), 145, "the standing visitor did not move");
        assert_eq!(g.spies[0].map_x, 130 - 2 * 12, "the host scrolled at two a frame");
        assert_eq!(g.spies[0].x(), 136 - 2 * 12, "and walked at two a frame");
    }

    /// Stage 12's fixture: player 0 the host with his view at `map` and his feet at `host_x`,
    /// player 1 nine units away on the same screen and the same row.
    fn two_on_one_screen(host_x: i32, other_x: i32, map: i32) -> Game {
        let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
        g.spies[0].screen = 4;
        g.spies[0].x3 = host_x * SUB;
        g.spies[0].y3 = 44 * SUB;
        g.spies[0].map_x = map;
        g.spies[1].screen = 4;
        g.spies[1].x3 = other_x * SUB;
        g.spies[1].y3 = 44 * SUB;
        g.spies[1].map_x = (other_x - 20).clamp(0, MAP_X_MAX);
        g
    }

    /// `DS:0x2EFC` **bounces** through 0, 2, 4, 2 — not a 0/2/4 wrap. The tail of
    /// `FUN_26c0_0c93` (`CS:0a06`) negates the step at both ends, so the both-legs-down frame (2)
    /// comes back between the two forward steps, which is the frame the owner was missing.
    #[test]
    fn walk_animation_bounces_through_the_standing_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let mut seq: Vec<u16> = Vec::new();
        for _ in 0..24 {
            let before = g.spies[0].frame;
            hold(&mut g, RIGHT, FRAME_TICKS as usize);
            if g.spies[0].frame != before || seq.is_empty() {
                seq.push(g.spies[0].frame);
            }
        }
        // 2 (standing) -> 4 -> 2 -> 0 -> 2 -> 4 ...
        assert_eq!(&seq[..6], &[4, 2, 0, 2, 4, 2], "the recorded triangle, got {seq:?}");
        // the step reverses at each end rather than wrapping
        let mut g = plain();
        let mut steps = Vec::new();
        for _ in 0..10 {
            hold(&mut g, RIGHT, FRAME_TICKS as usize);
            steps.push((g.spies[0].frame, g.spies[0].frame_step));
        }
        assert!(steps.iter().any(|&(_, s)| s == -ANIM_STEP), "the step must negate: {steps:?}");
        assert!(steps.iter().all(|&(f, _)| f <= ANIM_MAX));
    }

    /// Releasing the keys returns the legs to the standing frame (`FUN_26c0_3396` sets
    /// `DS:0x2EFC` = 2 and the step to 0), so a spy never stops with a foot in the air.
    #[test]
    fn releasing_the_keys_puts_the_legs_back_on_the_standing_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        // walk until the legs are on a forward frame
        let mut ticks = 0;
        while g.spies[0].frame == STAND_FRAME && ticks < 200 {
            hold(&mut g, RIGHT, FRAME_TICKS as usize);
            ticks += FRAME_TICKS as usize;
        }
        assert_ne!(g.spies[0].frame, STAND_FRAME, "a forward frame is showing");
        hold(&mut g, Keys::default(), FRAME_TICKS as usize);
        assert_eq!(g.spies[0].frame, STAND_FRAME, "the standing frame comes back at once");
        assert_eq!(g.spies[0].frame_step, 0);
    }

    /// The frame advances **once per game frame** (10 a second), and the spy's own step is 1 unit
    /// a frame — so a second of walking is 10 frames; two units a frame while the view scrolls.
    #[test]
    fn a_game_frame_is_six_ticks_of_one_unit() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let (x0, f0) = (g.spies[0].x(), g.spies[0].frame);
        // inside the dead zone (the view does not move yet) the spy walks 1 unit a frame
        hold(&mut g, RIGHT, FRAME_TICKS as usize - 1);
        assert_eq!(g.spies[0].x(), x0, "nothing happens before the frame boundary");
        hold(&mut g, RIGHT, 1);
        assert_eq!(g.spies[0].x(), x0 + 1, "one unit on the frame boundary");
        assert_ne!(g.spies[0].frame, f0, "and the legs move with it");
    }

    /// The world row's bounds (`DS:0x32B3`/`0x32B7` = 0..304) and the band's walls. Walking east
    /// from the room runs into the level-8 band (code 1, walkable) and then the row's own end;
    /// walking up or down stops on the mask's solid row unless the door column is under the spy.
    #[test]
    fn movement_is_bounded_by_the_band_and_the_row() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { start_x: Some(120), start_screen: Some(4), ..RoundOpts::default() });
        hold(&mut g, RIGHT, 60 * 60);
        assert!(g.spies[0].x() <= terrain::X_MAX, "x={}", g.spies[0].x());
        hold(&mut g, LEFT, 60 * 60);
        assert!(g.spies[0].x() >= terrain::X_MIN, "x={}", g.spies[0].x());
        // vertically: the mask's top and bottom rows are solid away from a door, so the spy parks
        // on the row above them (y 36) instead of reaching the band's y = 34
        let mut g = game(RoundOpts { start_x: Some(120), start_screen: Some(4), ..RoundOpts::default() });
        hold(&mut g, UP, 600);
        assert_eq!(g.spies[0].y(), Y_MIN + 2, "stopped by the solid row at y = 34");
        assert_eq!(g.spies[0].screen, 4, "no up door at x=120, so no room change");
        assert_eq!(g.spies[0].frame, STAND_FRAME, "and the legs are back on the standing frame");
        hold(&mut g, DOWN, 600);
        assert_eq!(g.spies[0].y(), Y_MAX - 2, "the bottom row is solid too");
        assert_eq!(g.spies[0].screen, 4);
    }

    /// **The mask veto**: the terrain code at the candidate spot decides whether the spy moves.
    /// Level 4 in island 1's room is a rock — its mask is `T_SOLID` from x-offset 16 to 21 of the
    /// cell (x 198..203 at column 7), so a spy walking east out of the snowfield stops at 197.
    #[test]
    fn a_solid_terrain_cell_refuses_the_move() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { start_x: Some(196), start_screen: Some(4), ..RoundOpts::default() });
        assert_eq!(g.levels[2][7], 4, "island 1 room 4 column 7 is the rock");
        assert!(g.terrain.solid(&g.levels, 4, 198, 44), "the rock's first solid unit");
        assert!(!g.terrain.solid(&g.levels, 4, 197, 44), "and the unit before it is open");
        hold(&mut g, RIGHT, 60 * 2);
        assert_eq!(g.spies[0].x(), 197, "the spy stops against the rock");
        assert_eq!(g.spies[0].frame, STAND_FRAME, "and stands, feet down");
        // the movement is refused, not delayed: another second changes nothing
        hold(&mut g, RIGHT, 60);
        assert_eq!(g.spies[0].x(), 197);
    }

    #[test]
    fn door_needs_the_right_column_and_the_right_level() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // on the door column (x 141 = cell 5 offset 11): up door -> screen 6
        let mut g = game(RoundOpts { start_x: Some(141), ..RoundOpts::default() });
        g.levels[2][5] = 2; // force the plain (non-icicle) variant
        until(&mut g, UP, 400, |g| g.spies[0].screen != 4);
        assert_eq!(g.spies[0].screen, 6, "up door into screen 6");
        assert_eq!(g.spies[0].y(), DOOR_ENTRY_FROM_BELOW_Y);

        // the recorded direction change: walking down out of screen 6 comes back to screen 4
        until(&mut g, DOWN, 400, |g| g.spies[0].screen != 6);
        assert_eq!(g.spies[0].screen, 4);
        // **stage 24e** — the crossing pass is a normal pass: the door arm writes the entry row
        // (`0x8e7`: `y = 0x32BB` = 34) and then `0xa1a` adds the frame's velocity, so the spy is on
        // 36 at the end of it. The recording's `demo-full-24c` ds_00096 (frame 202) holds 36.
        assert_eq!(g.spies[0].y(), DOOR_ENTRY_FROM_ABOVE_Y + 2);

        // level 24 is the icicle variant of the same door and works identically
        let mut g = game(RoundOpts { start_x: Some(141), ..RoundOpts::default() });
        g.levels[2][5] = 24;
        until(&mut g, UP, 400, |g| g.spies[0].screen != 4);
        assert_eq!(g.spies[0].screen, 6);

        // one cell to the left is wall, not door
        let mut g = game(RoundOpts { start_x: Some(120), ..RoundOpts::default() });
        hold(&mut g, UP, 400);
        assert_eq!(g.spies[0].screen, 4);
        assert_eq!(g.spies[0].y(), Y_MIN + 2, "the solid row above stops him, not a door test");
        // and a plain floor level is not a door even on the right column
        let mut g = game(RoundOpts { start_x: Some(141), ..RoundOpts::default() });
        g.levels[2][5] = 8;
        hold(&mut g, UP, 400);
        assert_eq!(g.spies[0].screen, 4);
        assert_eq!(g.spies[0].y(), Y_MIN + 2);
    }

    /// **The owner's defect A.** Walking west along the door row into the igloo's alcove must take
    /// the spy inside — no key. `FUN_26c0_1e0f`'s `AX == 7` branch fires on the *candidate* cell
    /// (the mask's code 7 at x-offset 20), sets `screen += 10`, `x = colbase + 0xF`, `y = 0x30`,
    /// `map_x = colbase - 8`, and the caller's commit adds the frame's own velocity — so a spy
    /// walking west lands on `colbase + 14`, which is what all 36 recorded entries show.
    #[test]
    fn walking_onto_the_igloo_door_cell_enters() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = 26 * 6;
        // start in the alcove (x-offset 22) at the door row, walking west
        let mut g = game(RoundOpts {
            start_screen: Some(4),
            start_x: Some(base + 22),
            start_y: Some(44),
            ..RoundOpts::default()
        });
        g.levels[2][6] = LEVEL_DOOR_IGLOO;
        // two frames: one step into the alcove, then the step onto the code-7 cell
        let n = until(&mut g, LEFT, 8 * FRAME_TICKS as usize, |g| g.in_interior(0));
        assert!(g.in_interior(0), "walking into the door must enter, without a key");
        assert_eq!(n, 2 * FRAME_TICKS as usize, "the door is the second step");
        assert_eq!(g.spies[0].screen, 14, "the igloo's interior is screen + 10");
        assert_eq!((g.spies[0].x(), g.spies[0].y()), (base + 14, 48),
                   "x = colbase + 0xF then the commit's one unit west, y = 0x30");
        assert_eq!(g.spies[0].map_x, base - 8, "the interior view: colbase - 8");
        // and it is not a teleport: one step, then the spy is inside and free to walk
        assert_eq!(g.spies[0].x3 % SUB, 0);
    }

    /// The same walk in the other direction (north into the alcove's row) also fires, because the
    /// trigger is the cell, not a row: the mask has code 7 on rows y = 40..45 at x-offset 20, and
    /// the interior landing is always `colbase + 0x30`-relative.
    #[test]
    fn the_igloo_door_is_a_cell_not_a_key() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = 26 * 6;
        for y in [40, 42, 44] {
            let mut g = game(RoundOpts {
                start_screen: Some(4),
                start_x: Some(base + 22),
                start_y: Some(y),
                ..RoundOpts::default()
            });
            g.levels[2][6] = LEVEL_DOOR_IGLOO;
            until(&mut g, LEFT, 8 * FRAME_TICKS as usize, |g| g.in_interior(0));
            assert_eq!((g.spies[0].screen, g.spies[0].x(), g.spies[0].y()),
                       (14, base + 14, 48), "walking west at y {y}");
        }
        // the body is still solid: from the west the spy stops at x-offset 0 of the cell
        let mut g = game(RoundOpts {
            start_screen: Some(4),
            start_x: Some(base - 10),
            start_y: Some(44),
            ..RoundOpts::default()
        });
        hold(&mut g, RIGHT, 20 * FRAME_TICKS as usize);
        assert_eq!(g.spies[0].screen, 4, "the body does not open");
        assert_eq!(g.spies[0].x(), base, "stopped on the cell's open edge unit");
        // the predicate the move path uses for both igloo codes
        assert!(g.terrain_solid(4, base + 5, 44), "the body is a solid code");
        assert!(!g.terrain_solid(4, base + 21, 44), "the alcove is walkable");
    }

    /// `FUN_26c0_1e0f`'s guard: `if (other.screen > 9 && other.screen - 10 == this.screen)` the door
    /// refuses and the spy does not move — one spy per igloo.
    #[test]
    fn a_spy_inside_blocks_the_igloo_door() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = 26 * 6;
        let mut g = game(RoundOpts {
            players: 2,
            start_screen: Some(4),
            start_x: Some(base + 22),
            start_x2: Some(40),
            start_y: Some(44),
            ..RoundOpts::default()
        });
        g.levels[2][6] = LEVEL_DOOR_IGLOO;
        // the other spy is already inside this igloo
        g.spies[1].screen = 14;
        g.spies[1].x3 = 140 * SUB;
        g.spies[1].y3 = 48 * SUB;
        hold(&mut g, LEFT, 4 * FRAME_TICKS as usize);
        assert_eq!(g.spies[0].screen, 4, "blocked while the igloo is occupied");
        assert_eq!(g.spies[0].x(), base + 21, "and he stands still at the alcove's edge");
        // a different igloo's interior does not block this door
        g.spies[1].screen = 16;
        until(&mut g, LEFT, 8 * FRAME_TICKS as usize, |g| g.in_interior(0));
        assert_eq!((g.spies[0].screen, g.spies[0].x()), (14, base + 14));
    }

    /// The **interior** masks carry a code-7 cell too — a mirrored copy of the exterior door in the
    /// interior's far alley (level 18/22, x-offset 20, y 40..42). `FUN_26c0_1e0f` would fire there as
    /// well, sending the spy to `screen + 10` = 24/26/28, past the end of the 10-row level table: the
    /// original would read a garbage pointer. The port treats it as inert and says so. No recording
    /// goes near it: of 5,106 interior samples in the corpus, none is at an x-offset above 18.
    #[test]
    fn the_igloo_interiors_mirrored_door_cell_is_inert() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = 26 * 6;
        let mut g = game(RoundOpts {
            start_screen: Some(14),
            start_x: Some(base + 22),
            start_y: Some(42),
            ..RoundOpts::default()
        });
        hold(&mut g, LEFT, 8 * FRAME_TICKS as usize);
        assert_eq!(g.spies[0].screen, 14, "no second transition inside the igloo");
        assert_eq!(g.spies[0].x(), base + 20, "he walks onto the cell and stops at the wall behind");
    }

    /// The stage-4 reading is still reachable (`--igloo-entry key`), and `off` leaves the cell inert
    /// — so the owner can compare the three behaviours with one flag instead of a rebuild.
    #[test]
    fn the_igloo_entry_flag_selects_the_model() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = 26 * 6;
        let opts = |mode| RoundOpts {
            start_screen: Some(4),
            start_x: Some(base + 22),
            start_y: Some(44),
            igloo_entry: mode,
            ..RoundOpts::default()
        };
        // walk: enters
        let mut g = game(opts(IglooEntry::Walk));
        g.levels[2][6] = LEVEL_DOOR_IGLOO;
        until(&mut g, LEFT, 8 * FRAME_TICKS as usize, |g| g.in_interior(0));
        assert_eq!((g.spies[0].screen, g.spies[0].x()), (14, base + 14));
        // off: the alcove is a dead end, the code-7 cell is walkable and inert
        let mut g = game(opts(IglooEntry::Off));
        g.levels[2][6] = LEVEL_DOOR_IGLOO;
        hold(&mut g, LEFT, 8 * FRAME_TICKS as usize);
        assert_eq!(g.spies[0].screen, 4, "off: no transition");
        assert!(g.spies[0].x() <= base + 20, "and the spy walks through the cell");
        // key: the stage-4 path, from the alcove, with the action key
        let mut g = game(opts(IglooEntry::Key));
        g.levels[2][6] = LEVEL_DOOR_IGLOO;
        hold(&mut g, FIRE, FRAME_TICKS as usize);
        assert_eq!((g.spies[0].screen, g.spies[0].x(), g.spies[0].y()), (14, base + 15, 48),
                   "key: colbase + 0xf with no move to commit");
    }

    #[test]
    fn clock_counts_down_and_expiry_ends_the_round() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        assert_eq!(g.clock_secs(), 360);
        assert_eq!(g.clock_left, 360, "`DS:0x324D` is the count itself");
        // one count is 9 frames (`CS:0680`), i.e. 975.1 ms: 360 counts = 3,240 frames = 351 s
        for _ in 0..(CLOCK_FRAMES * FRAME_TICKS) as usize {
            g.tick(Input::default());
        }
        assert_eq!(g.clock_secs(), 359);
        assert_eq!(g.phase, Phase::Playing);
        // burn the rest: the round ends with no winner and no further movement
        hold(&mut g, Keys::default(), (360 * CLOCK_FRAMES * FRAME_TICKS) as usize);
        assert_eq!(g.phase, Phase::Over);
        assert_eq!(g.end, Some(RoundEnd::Clock), "the clock is one of the three exits");
        assert_eq!(g.clock_secs(), 0);
        let x = g.spies[0].x();
        hold(&mut g, RIGHT, 60);
        assert_eq!(g.spies[0].x(), x, "a finished round does not move");
    }

    // ---- stage 16a: the game frame's period ---------------------------------------------------
    //
    // The frame-sync loop waits for a **timer word** (`CS:063c`), and the timer is not the BIOS'
    // 18.2 Hz: `CS:4574`/`CS:4578` reprogram PIT channel 0 to divisor 0x136C (4972) —
    // 1193181.8182 / 4972 = 239.98 Hz — and `CS:4562` points INT 1C at `CS:4196`, which increments
    // `DS:0x33F4` on every one. The loop waits for `(DS:0x33F4 & 0x7F) >= 0x1A` with the word zeroed
    // at the top of each frame (`CS:0658`), so a frame is **26 ticks = 108.34 ms**, and the wait is a
    // *minimum* (a frame whose body outlasts 26 ticks runs long by whole ticks, never short).

    #[test]
    fn the_frame_is_twenty_six_ticks_of_a_240_hz_tick() {
        assert_eq!(TICK_HZ, 240,
                   "CS:4574/CS:4578 program PIT channel 0 to 0x136C (4972): 239.98 Hz, not 18.2");
        assert_eq!(FRAME_TICKS, 26,
                   "CS:0642 waits for the tick word to reach 0x1A (26) from zero");
        let secs = FRAME_TICKS as f64 / TICK_HZ as f64;
        assert!((secs * 1000.0 - 108.34).abs() < 0.02,
                "26 ticks at 239.98 Hz is 108.34 ms (two BIOS ticks would be 109.85): {:.3} ms",
                secs * 1000.0);
    }

    #[test]
    fn a_frame_is_the_twenty_sixth_tick_and_not_the_sixth() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let x = g.spies[0].x();
        hold(&mut g, RIGHT, 25);
        assert_eq!(g.spies[0].x(), x, "25 ticks is not a frame — nothing has moved yet");
        hold(&mut g, RIGHT, 1);
        assert_eq!(g.spies[0].x(), x + WALK_X_PER_FRAME,
                   "the 26th tick completes the frame, and the frame is where the step happens");
    }

    #[test]
    fn the_round_clock_counts_nine_frames_a_second() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // `CS:0680`: `cmpw $0xdc,0x33f6` — the word is +26 a frame and compared with 220 once a
        // frame, then cleared when it fires. 8 frames (208) are not enough; 9 (234) are.
        let mut g = plain();
        let start = g.clock_secs();
        for _ in 0..8 {
            hold(&mut g, Keys::default(), FRAME_TICKS as usize);
        }
        assert_eq!(g.clock_secs(), start, "eight frames (208 ticks) do not reach 0xDC + 1");
        hold(&mut g, Keys::default(), FRAME_TICKS as usize);
        assert_eq!(g.clock_secs(), start - 1, "the ninth frame (234 ticks) does");
    }

    #[test]
    fn a_full_round_is_three_thousand_two_hundred_and_forty_frames() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        assert_eq!(g.clock_secs(), 360, "island 1's own clock");
        let mut frames = 0;
        while g.clock_secs() > 0 && frames < 8000 {
            hold(&mut g, Keys::default(), FRAME_TICKS as usize);
            frames += 1;
        }
        assert_eq!(frames, 360 * 9, "360 counts x 9 frames = 3,240 frames = 351.0 s of wall clock");
    }

    #[test]
    fn a_tap_on_the_last_tick_of_a_frame_still_moves_the_spy() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // stage 8b's latch: the frame takes the union of every tick's keys, so a press that starts
        // and ends between two frames is not lost — with 26 samples a frame there is more room for
        // that than there was at 6, not less.
        let mut g = plain();
        let x = g.spies[0].x();
        hold(&mut g, Keys::default(), FRAME_TICKS as usize - 1);
        hold(&mut g, RIGHT, 1);
        assert_eq!(g.spies[0].x(), x + WALK_X_PER_FRAME, "the last tick's tap is latched");
    }

    /// The frame length is a **flag**, so the period can be swept without touching anything else:
    /// `RoundOpts::frame_ticks` (the `--fps` command line) sets the wait, and the clock's tick word
    /// follows it because it accumulates the frame's own ticks — 24 ticks is 10 fps and a count
    /// every ceil(221/24) = 10 frames, exactly as the original's arithmetic would do it.
    #[test]
    fn the_frame_length_can_be_swept_and_the_clock_follows_it() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // 26 ticks (the original): 9 frames a count
        let mut g = plain();
        assert_eq!(g.frame_ticks, FRAME_TICKS);
        assert_eq!((CLOCK_TICKS_WORD as u32).div_ceil(FRAME_TICKS), CLOCK_FRAMES);
        let mut f = plain();
        f.frame_ticks = 24;              // 10 frames a second, the port's old period
        let start = f.clock_secs();
        for _ in 0..9 {
            hold(&mut f, Keys::default(), 24);
        }
        assert_eq!(f.clock_secs(), start, "24 x 9 = 216 ticks is still under 0xDC + 1");
        hold(&mut f, Keys::default(), 24);
        assert_eq!(f.clock_secs(), start - 1, "the tenth frame (240 ticks) is the count");
        // and the sweep is a single field, so a plain round still uses the original's 26
        hold(&mut g, Keys::default(), 8 * FRAME_TICKS as usize);
        assert_eq!(g.clock_secs(), 360);
        hold(&mut g, Keys::default(), FRAME_TICKS as usize);
        assert_eq!(g.clock_secs(), 359);
    }

    /// The recorded clock, as the independent measurement of the frame period: the timer ROI
    /// changes once a **count**, and a count is 9 frames, so the mean interval / 9 is the frame
    /// period of the machine that produced the recording.
    #[test]
    fn the_recorded_clock_counts_nine_frames_a_second() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sess = crate::orig::require("recon/round/sessions");
        if !sess.exists() {
            eprintln!("skip: no recorded sessions");
            return;
        }
        let port_frame = FRAME_TICKS as f64 / TICK_HZ as f64;
        let mut seen = Vec::new();
        for dir in std::fs::read_dir(&sess).expect("sessions") {
            let dir = dir.expect("entry").path();
            let Ok(text) = std::fs::read_to_string(dir.join("roi_series.csv")) else { continue };
            let mut last: Option<(f64, String)> = None;
            let mut iv: Vec<f64> = Vec::new();
            for line in text.lines().skip(1) {
                let f: Vec<&str> = line.split(',').collect();
                if f.len() < 3 || f[1].trim() != "timer" {
                    continue;
                }
                let Ok(mono) = f[0].trim().parse::<f64>() else { continue };
                let v = f[2].trim().to_string();
                if let Some((t, ref was)) = last {
                    if *was != v {
                        if (0.5..3.0).contains(&(mono - t)) {
                            iv.push(mono - t);
                        }
                        last = Some((mono, v.clone()));
                    }
                } else {
                    last = Some((mono, v));
                }
            }
            if iv.len() < 100 {
                continue;
            }
            let mean = iv.iter().sum::<f64>() / iv.len() as f64;
            seen.push((dir.file_name().unwrap().to_string_lossy().to_string(), iv.len(), mean));
        }
        assert!(seen.len() >= 4, "at least four sessions with a clock, got {}", seen.len());
        for (name, n, mean) in &seen {
            let frame = mean / 9.0;
            assert!((frame - port_frame).abs() < 0.002,
                    "{name}: {n} counts at {:.3} ms each -> a frame of {:.3} ms, the port says {:.3}",
                    mean * 1000.0, frame * 1000.0, port_frame * 1000.0);
        }
    }

    #[test]
    fn scroll_keeps_the_spy_inside_the_dead_zone() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { start_x: Some(120), start_screen: Some(4), ..RoundOpts::default() });
        // the round-start rule: the scroll is set to x - 20
        assert_eq!(g.spies[0].map_x, 120 - MAP_X_ON_ENTRY_OFFSET);
        // walking east, the igloo's solid body (level 16 at column 6) stops the spy at x = 156
        for _ in 0..60 {
            hold(&mut g, RIGHT, FRAME_TICKS as usize);
            let s = &g.spies[0];
            if s.map_x > 0 && s.map_x < MAP_X_MAX {
                assert!(s.map_x >= s.x() - SCROLL_RIGHT_EDGE - 1 && s.map_x <= s.x() - SCROLL_LEFT_EDGE + 1,
                        "x={} map_x={}", s.x(), s.map_x);
            }
        }
        assert_eq!(g.spies[0].x(), 156, "the igloo blocks the way east");
        // west, the walk continues and the view follows it back down
        let map_before = g.spies[0].map_x;
        // 30 frames (stage 16a: a frame is 26 ticks; this used to be 180 ticks of a 60 Hz tick)
        hold(&mut g, LEFT, 30 * FRAME_TICKS as usize);
        assert!(g.spies[0].x() < 156 && g.spies[0].map_x < map_before,
                "the view has followed him west: x={} map_x={}", g.spies[0].x(), g.spies[0].map_x);
    }

    #[test]
    fn island_one_round_walks_between_its_two_rooms() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // the acceptance path: from the snowfield, walk east to the door column, up into screen 6,
        // then down again into screen 4.
        let mut g = game(RoundOpts { start_x: Some(90), start_screen: Some(4), ..RoundOpts::default() });
        assert_eq!(g.spies[0].screen, 4);
        assert_eq!(g.spies[0].x(), 90);
        let mut ticks = 0;
        while g.spies[0].x() < 141 && ticks < 5000 {
            g.tick(Input { p1: RIGHT, p2: Keys::default() });
            ticks += 1;
        }
        // steps are one or two units a frame, so the door column can be overshot by one
        assert!((141..=142).contains(&g.spies[0].x()), "walked to the door column: {}", g.spies[0].x());
        let mut rooms = vec![g.spies[0].screen];
        for _ in 0..400 {
            g.tick(Input { p1: UP, p2: Keys::default() });
            if rooms.last() != Some(&g.spies[0].screen) {
                rooms.push(g.spies[0].screen);
            }
        }
        assert_eq!(rooms.get(1), Some(&6), "up door into screen 6 (rooms: {rooms:?})");
        // now walk back down through the other door
        let mut back = vec![g.spies[0].screen];
        for _ in 0..400 {
            g.tick(Input { p1: DOWN, p2: Keys::default() });
            if back.last() != Some(&g.spies[0].screen) {
                back.push(g.spies[0].screen);
            }
        }
        assert_eq!(back.get(1), Some(&4), "down door back into screen 4 (rooms: {back:?})");
    }

    #[test]
    fn strip_is_composed_and_the_marker_lands_on_the_spy() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = plain();
        let strip = g.strip_for(0, true);
        let s = &g.spies[0];
        let sx = s.x() - s.map_x;
        let row = (s.y() - 1) as usize;
        // the P1 marker is a magenta/black 2-byte bar: magenta on the left, black on the right
        assert_eq!(strip[row * STRIP_PITCH + sx as usize], 0b10_10_10_10, "marker ink at the spy's cell");
        assert_eq!(strip[row * STRIP_PITCH + sx as usize + 1], 0b00_00_00_00);
        // without the marker the same bytes come straight from the room plates
        let bare = g.strip_for(0, false);
        assert_ne!(bare[row * STRIP_PITCH + sx as usize], strip[row * STRIP_PITCH + sx as usize]);
        // and the frame puts it at the console's viewport
        let frame = g.frame(true);
        assert_eq!(frame.idx.len(), crate::PIXELS);
        let (vx, vy) = round::VIEW_TOP;
        assert_eq!(frame.idx[(vy + row) * crate::W + vx + sx as usize * 4],
                   strip[row * STRIP_PITCH + sx as usize] & 3);
    }

    #[test]
    fn engine_view_is_the_origins_own_composition() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // A recorded dump composed twice: once from the dump itself (stage 1's exact path) and once
        // from this engine's state with the same screen / scroll / level table. If the engine's
        // data-segment patching is faithful the strips are byte-identical — that is the engine
        // drawing the room the original drew, not an approximation of it.
        let dump = crate::orig::require("recon/round/sessions/playtest1/ds_00521.bin");
        if !dump.exists() {
            eprintln!("skip: {} not present", dump.display());
            return;
        }
        let st = RoundState::from_ds_dump(&dump).expect("dump");
        let mut g = game(RoundOpts { start_screen: Some(4), start_x: Some(120), ..RoundOpts::default() });
        g.objects = false;
        g.spies_layer = false;
        g.levels = st.levels;
        g.spies[0].map_x = st.players[0].map_x as i32;
        g.spies[0].screen = st.players[0].band;
        let mine = g.strip_for(0, false);
        let art = Art::from_exe(&exe()).unwrap();
        let (theirs, passes) = round::compose_passes(&art, &st, 0);
        assert_eq!(passes.len(), 3);
        assert_eq!(mine.to_vec(), theirs.to_vec(), "engine strip != the dump's own composition");
    }

    #[test]
    fn script_parser_reads_ticks_and_repeats() {
        let steps = parse_script("# a comment\nright *3\nup fire\nwait\n").unwrap();
        assert_eq!(steps.len(), 5);
        assert!(steps[0].p1.right && !steps[0].p1.up);
        assert!(steps[3].p1.up && steps[3].p1.fire);
        assert!(!steps[4].p1.any());
        assert!(parse_script("sideways").is_err());
    }

    // ---- stage 7: the items ---------------------------------------------------------------

    /// One game frame is `FRAME_TICKS` ticks; these helpers drive whole frames.
    fn fire_frames(g: &mut Game, keys: Keys, frames: usize) {
        for _ in 0..frames {
            for _ in 0..FRAME_TICKS {
                g.tick(Input { p1: keys, p2: Keys::default() });
            }
        }
    }
    const NOKEYS: Keys = Keys { up: false, down: false, left: false, right: false, fire: false, fire_b: false };

    /// Put `byte` in the object cell under player 1 and return its address. The cell is the one
    /// `FUN_26c0_2549` computes, so this is the same place the original would look.
    fn put_underfoot(g: &mut Game, byte: u8) -> usize {
        let a = g.cell_of(0).expect("the spy stands on an object cell");
        g.ds[a] = byte;
        a
    }

    /// The same for player `p` (`SI` = 0 or 2 in the original's own units).
    fn put_underfoot_for(g: &mut Game, p: usize, byte: u8) -> usize {
        let a = g.cell_of(p).expect("the spy stands on an object cell");
        g.ds[a] = byte;
        a
    }

    fn kit_bit(code: u16) -> u16 {
        let exe = exe();
        items::code_bit(&exe[DS_FILE_OFF..], code)
    }

    /// **measured + read** — tapping fire on a mound takes what is under it and clears the byte.
    /// 171 recorded digs do exactly this, 170 of them from the cell under the spy or one entry
    /// ahead (`items_events.py`), e.g. `playtest5/ds_00014 -> 15`: mound `0x89` gone, carried 9,
    /// kit 0x83A -> 0x87A.
    #[test]
    fn fire_on_a_mound_digs_the_item_out() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let a = put_underfoot(&mut g, 0x89);
        let kit0 = g.spies[0].kit;
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.spies[0].carried, 9, "the mound's code is its item code");
        assert_eq!(g.ds[a], 0, "the mound byte is cleared");
        assert_eq!(g.spies[0].kit, kit0 | 0x40, "part 9's kit mirror bit (0x2D60[9])");
        assert_eq!(g.mask, MASK_BASE, "a pickup does not touch the deposited mask");
        // **stage 25a** — `CS:38bf..38c7` is a *raise*: the take loads the item's own set into
        // `0x2EF8`, calls `FUN_26c0_277c`, and that sets `0x32CF = 0x32CB = 0`, switches the drawn
        // set to `word[0x2CC4 + 0]` and seeds the frame word at -2. The item's set is saved
        // (`0x32A7`) and comes back when `CS:2807` ends the state two frames later.
        assert!(g.spies[0].pose_raised, "a take raises the spy (`0x2F18` = 1)");
        assert_eq!(g.spies[0].pose_state, round::STATE_TAKE, "`0x32CF` = the take's state 0");
        assert_eq!(g.spies[0].set, round::anim_set(&exe()[DS_FILE_OFF..], round::STATE_TAKE),
                   "the take's own pose set (`word[0x2CC4 + 0]` = 132)");
        assert_eq!(g.spies[0].saved_set, items::code_pose(&exe()[DS_FILE_OFF..], 9),
                   "`CS:27a8`: the item's set is what the take saves");
        // two frame bodies later the state ends and the carrying pose is what is drawn again
        for _ in 0..2 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input::default());
            }
        }
        assert!(!g.spies[0].pose_raised, "the take's state ends after two frames");
        assert_eq!(g.spies[0].set, items::code_pose(&exe()[DS_FILE_OFF..], 9),
                   "`CS:2807`: the spy is drawn with the carrying pose again");
    }

    /// **Stage 25a — the take is a raise, and a raise holds the base counter.**
    ///
    /// `CS:38bf` loads the item's own pose set into `0x2EF8` and calls `FUN_26c0_277c`, which is the
    /// animation start: `0x2F18` = 1, `0x32CF` = `0x32CB` (0, the take path's own write), the drawn
    /// set to `word[0x2CC4 + 0]`, `0x2EFC` = -2. The frame body that took the item returned through
    /// `CS:78e` → `0x836` → `0x97c` → `0x982`, so it **steps** the new state once (frame 0), and the
    /// two frames after it are spent raised: `CS:0727` sends them past the heat block, so `0x2F54`
    /// does not tick (and the keys are not read).
    ///
    /// That is the demo's frame 14: the recording's counter stops dead for two frames there (base
    /// 11 → 18 over frames 12..21, 45 → 50 over 688..695), which is what puts its 138-frame drain at
    /// 341 instead of 339.
    #[test]
    fn a_take_raises_the_spy_for_two_frames_and_holds_the_base_counter() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        put_underfoot(&mut g, 0x89);
        let before = g.spies[0].base_t;
        fire_frames(&mut g, FIRE, 1);
        assert!(g.spies[0].pose_raised, "`CS:38c7` raises him");
        assert_eq!(g.spies[0].pose_state, round::STATE_TAKE);
        assert_eq!(g.spies[0].base_t, before + 1,
                   "the take's own frame ticks: `CS:074f` runs before the key is read");
        let at_take = g.spies[0].base_t;
        fire_frames(&mut g, NOKEYS, 1);
        assert!(g.spies[0].pose_raised, "the state's second drawn frame");
        assert_eq!(g.spies[0].base_t, at_take, "a raised spy skips `CS:074f` (`CS:0727`)");
        fire_frames(&mut g, NOKEYS, 1);
        assert!(!g.spies[0].pose_raised, "`0x2EFC` 2 + 2 > `0x2CE6[0]` = 2 ends it");
        assert_eq!(g.spies[0].base_t, at_take, "and the frame it ends in is skipped too");
        fire_frames(&mut g, NOKEYS, 1);
        assert_eq!(g.spies[0].base_t, at_take + 1, "the frame after it ticks again");
    }

    /// **Stage 25a — `FUN_26c0_2392` reads `DS:0x32D7`, the cell cached at the end of the previous
    /// frame** (`FUN_26c0_2549` at `CS:0afd`), not the position the spy is standing on now. The
    /// touch therefore fires one frame *after* the spy steps onto the hatch — which is exactly what
    /// the demo's dumps show (spy 1 reaches x 116 during frame 806 and `0x32CF` only becomes 30 at
    /// 807) and what makes the whole descent/ascent land on the recording's frames.
    ///
    /// Fails against the stage-24g build, which opened the hatch in the arrival frame and led the
    /// recording by one (`p1.y` first diverged at 806, `p2.y` at 822).
    #[test]
    fn the_hatch_touch_reads_the_cell_cached_at_the_end_of_the_previous_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        let cell = g.cell_of(0).expect("the start option stands him on an object cell");
        assert!(matches!((g.ds[cell] & items::CODE_MASK) as u16, 0x0F | 0x10 | 0x11 | 0x15),
                "the start option stands him on the hatch (`{:#04x}`)", g.ds[cell]);
        // the spy is on the hatch, but the cached pointer still holds the cell he *stepped from* —
        // the state `CS:0afd` would have left at the end of the frame he walked in on
        let old = cell - 1;
        g.ds[old] = 0;
        g.feet_addr[0] = Some(old);
        g.feet_prev[0] = Some(old);
        assert_eq!(g.hatch.seq, HatchSeq::Idle);
        let frames = |g: &mut Game, n: usize| {
            for _ in 0..n {
                hold(g, NOKEYS, FRAME_TICKS as usize);
            }
        };
        frames(&mut g, 1);
        assert_eq!(g.hatch.seq, HatchSeq::Idle,
                   "the touch read the cell he left, not the one he is standing on");
        assert_eq!(g.spies[0].pose_state, 0, "and he is still not in the descent");
        // the end of that body cached the hatch's own cell, so the next frame's touch fires
        frames(&mut g, 1);
        assert_eq!(g.hatch.seq, HatchSeq::Descend, "the next frame's touch sees the hatch");
        assert_eq!(g.spies[0].pose_state, round::STATE_DESCEND, "`0x32CF` = 0x1E");
    }

    /// The same for a *visible* item, and for the shift of the source: the code looks under the
    /// feet first and one entry ahead only when the feet cell is empty (`FUN_26c0_373b`).
    #[test]
    fn the_item_one_entry_ahead_is_taken_when_the_feet_are_empty() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let a = g.cell_of(0).unwrap();
        g.ds[a] = 0;
        let ahead = if g.spies[0].mirror { a - 1 } else { a + 1 };
        let a = ahead;
        g.ds[a] = 0x08;
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.spies[0].carried, 8);
        assert_eq!(g.ds[a], 0);
    }

    /// Codes above 12 are not items: the hatch (`0x10`), the bear (`0x11`) and the hatch's closed
    /// byte (`0x0F`) cannot be picked up (`CMP AL,0xC; JG return` at `CS:376b`).
    ///
    /// **Stage 13a**: `0x0E` and `0x14` are *hazards* — the fire press is not what consumes them,
    /// `FUN_26c0_2444` is, and it fires on the frame the spy is standing on one (see the hazard
    /// tests below). They are still not items, which is what this test is about.
    #[test]
    fn the_hatch_and_the_hole_are_not_items() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for byte in [0x10u8, 0x0F, 0x11, 0x15] {
            let mut g = plain();
            let a = put_underfoot(&mut g, byte);
            fire_frames(&mut g, FIRE, 1);
            assert_eq!(g.spies[0].carried, CARRY_NONE, "code {byte:#04x} must not be taken");
            assert_eq!(g.ds[a], byte, "and its byte must be left alone");
        }
        // and the two hazard bytes are still refused by the pickup itself, one frame before the
        // hazard routine resolves them
        for byte in [0x0Eu8, 0x14] {
            let mut g = plain();
            let a = g.cell_of(0).unwrap();
            g.ds[a] = byte;
            let took = g.pickup(0);
            assert!(!took, "code {byte:#04x} is not an item");
            assert_eq!(g.spies[0].carried, CARRY_NONE);
            assert_eq!(g.ds[a], byte);
        }
    }

    // ---- stage 18f: the two fire buttons (`0x327f` A, `0x3283` B) --------------------------

    /// **stage 18f — the punch repeats.** `CS:129c` clears its own six key flags at the top of
    /// every frame (`12a0`..`12c8`) and the attack then sets button B again (`CS:1a46`), so an
    /// attacking computer re-presses B on every frame its behaviour runs; the frame body tests B as
    /// a *level* (`CS:0791`), so it throws, is owned by the eight-frame throw animation, and throws
    /// again. Measured: 19 throws in 200 frames after, 14 at HEAD — this is the *outcome* guard,
    /// not the red-at-HEAD case; the defect itself is pinned by the two tests below, and on the
    /// one-key build the count is **0** (STAGE18F_REPORT.md §3.3).
    #[test]
    fn a_computer_in_the_punch_repeats_the_throw() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, cpu: [false, true], difficulty: 1,
                                     start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(120), start_y: Some(44),
                                     ..RoundOpts::default() });
        // an empty room, so no terrain code can refuse the throw and no wall can block the walk
        g.levels = [[0u16; 12]; 10];
        g.spies[1].y3 = g.spies[0].y3;          // one row: inside the punch's |dy| <= 4
        // the visitor flag is the attack band's own gate (`CS:1cd9`); one frame of both spies
        // standing still is enough to set it
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        assert!(g.spies[0].blank || g.spies[1].blank,
                "the two spies share a room, which is what puts the AI in the attack band");
        // a throw is a new throw animation (`CS:302c` starts state 2, which owns the spy for
        // eight frames and releases the ball on frame 8)
        let mut throws = 0usize;
        let mut busy = false;
        for _ in 0..200 {
            fire_frames(&mut g, NOKEYS, 1);
            let now = g.spies[1].anim.map(|a| a.state) == Some(round::STATE_THROW);
            if now && !busy {
                throws += 1;
            }
            busy = now;
        }
        assert!(throws >= 12, "the punch should repeat (200 frames): {throws}");
    }

    /// **stage 20c — the computer's trapulator dwell, and the slot it opens on.**
    ///
    /// The recorded one-player rounds keep the panel up for **0.5–1.1 %** of a difficulty-1 round
    /// (4 rounds) and **0–1.1 %** of a difficulty-2 one (3 rounds) — 1–3 dumps per round against
    /// 1–5 trapulator errands — and every recorded panel frame that has an item request carries the
    /// cursor on **that request**: `ai-d2-18e` ds_00080 `0x2F3E = 0x2F98 = 10`, `m3-heat-90s`
    /// ds_00122 `2 / 2`, `audio-smoke` ds_00052 `10 / 10`, and the map errands show `0x2F3E = 12`
    /// (`ai-d2-18e` ds_00033). Never mid-walk.
    ///
    /// The port *walks*: `item_select` presses RIGHT until the cursor word matches, one slot per
    /// three frames (`DS:0x2F9E`'s repeat, which the disassembly really has at `CS:2af6`), so a
    /// bucket errand that starts from the ice pick's slot runs 15 frames of open panel and the
    /// share comes out 5–26× the recording's. `DS:0x2F3C` is not panel-only state — the stow
    /// writes it from the table at `0x2D7A` (`CS:31aa`) — so the errand places it on the slot it
    /// was asked for when it opens the panel and presses A on the next frame.
    ///
    /// Measured on the 20 d1/d2 runs below: every visit is 1 frame, the first frame of it carries
    /// the requested slot, and the share is inside the recorded band.
    #[test]
    fn the_computers_trapulator_opens_on_the_requested_slot_and_lasts_one_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut worst = 0.0f64;
        for (d, frames) in [(1u16, 539usize), (2, 571)] {
            for seed in 17..22u32 {
                let mut g = game(RoundOpts { players: 2, cpu: [false, true], difficulty: d, seed,
                                             ..RoundOpts::default() });
                let mut open = 0usize;
                let mut runs: Vec<usize> = Vec::new();
                let mut cur = 0usize;
                let mut bad: Vec<String> = Vec::new();
                for f in 0..frames {
                    // what the AI asked for on this frame, and whether the panel is already up
                    let want = g.cpu_state(1).and_then(|c| {
                        if c.intent[cpu::I_MAP] != 0 {
                            Some(0x0cu16)
                        } else if c.intent[cpu::I_ITEM] != 0 {
                            Some(c.target_slot)
                        } else {
                            None
                        }
                    });
                    let was_open = g.spies[1].trap_open;
                    fire_frames(&mut g, NOKEYS, 1);
                    let s = &g.spies[1];
                    if s.trap_open {
                        if !was_open && cur == 0 {
                            if let Some(want) = want.filter(|&w| w != 0) {
                                if s.trap_cursor != want {
                                    bad.push(format!(
                                        "d{d}/s{seed} f{f}: opens on cursor {} but asked for {want:#x}",
                                        s.trap_cursor));
                                }
                            }
                        }
                        cur += 1;
                        open += 1;
                    } else if cur > 0 {
                        runs.push(cur);
                        cur = 0;
                    }
                }
                if cur > 0 {
                    runs.push(cur);
                }
                assert!(bad.is_empty(), "{bad:?}");
                assert!(runs.iter().all(|&n| n <= 2),
                        "d{d}/s{seed}: visit lengths {runs:?} (the recording is 1–2 dumps)");
                let share = 100.0 * open as f64 / frames as f64;
                worst = worst.max(share);
                assert!(share <= 1.6,
                        "d{d}/s{seed}: panel on screen {share:.2} % of {frames} frames \
                         (the recorded band is 0.5–1.1 %)");
            }
        }
        // and the band is not just "under the ceiling": the port has to be on the same order
        assert!(worst >= 0.2, "no panel time at all ({worst:.2} %): the errands are not running");
    }

    /// **stage 20c — the base cold still reaches the freeze once the damage no longer does.**
    ///
    /// Stage 20b reported "the port's idle human never freezes" and measured the heat word at
    /// **5–9 over 405 s**. That is `spies[1]` — the *computer*, whose 138-frame base counter
    /// (`CS:074f`..`075b`) is cleared on every terrain step it takes (`FUN_26c0_21bd`'s tail), so it
    /// hovers low exactly as the recording's own computer does. The human is `spies[0]`
    /// (`DS:0x2F58`); it walks 3 → 63 in 30 steps and freezes, and this pins both halves.
    #[test]
    fn the_base_drain_alone_reaches_the_freeze_word() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // a round with no computer at all: nothing can be thrown, so only `CS:074f` can move the word
        // the clock has to outlast 30 steps of the 138-frame base drain (448 s of round time)
        let mut g = game(RoundOpts { players: 2, cpu: [false, false], difficulty: 1,
                                     clock_override: Some(600), ..RoundOpts::default() });
        let period = HEAT_BASE_PERIOD as usize;
        fire_frames(&mut g, NOKEYS, 29 * period);        // 29 steps of two cold, from 3
        assert_eq!(g.spies[0].heat, 61, "one step per 138 frames");
        fire_frames(&mut g, NOKEYS, period);             // the 30th
        assert_eq!(g.spies[0].heat, 63, "clamped at the freeze word");
        // `CS:0745` is tested at the top of the block, before `CS:074f` steps the counter, so the
        // freeze lands on the frame after the word passes 0x3D — the original's own ordering
        assert_eq!(g.spies[0].frozen, 0, "not yet: the test sits above the step");
        // the per-player loop runs player 2 first, so his freeze starts the laugh on player 1 and
        // `CS:2238`'s own first guard ("already animating: nothing at all") holds player 1's
        // freeze until the laugh ends — the order the `freeze-death` session records
        fire_frames(&mut g, NOKEYS, 1);
        assert_eq!(g.spies[1].frozen, 1, "the black spy freezes first");
        assert_eq!(g.spies[0].frozen, 0, "the white spy is laughing");
        fire_frames(&mut g, NOKEYS, 11);
        assert_eq!(g.spies[0].frozen, 1, "and then he freezes too (CS:0745 -> CS:2238)");
    }

    /// **stage 18f — a computer holding B does not bury its own briefcase.** With the two buttons
    /// collapsed into one flag every attacking frame also ran the *action* key: the briefcase went
    /// into the ground and was dug up again on the next frame (measured: 2,373 frames at
    /// (6,160,48), hand 12 → -1 → 12, cell `0x00` → `0x8C`). Red on that build; green here, where
    /// the attack presses B and the hand is put away once (`CS:1a07` → `0x315f`).
    #[test]
    fn a_computer_carrying_the_briefcase_does_not_bury_and_dig_it() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, cpu: [false, true], difficulty: 1,
                                     start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(120), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        g.spies[1].y3 = g.spies[0].y3;
        let feet = g.cell_of(1).expect("the spy stands on an object cell");
        g.set_carried(1, 12);                   // the briefcase, in the computer's hand
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        let mut digs = 0usize;
        let mut had_case = false;
        for _ in 0..60 {
            fire_frames(&mut g, NOKEYS, 1);
            let now = g.spies[1].carried == 12;
            if now && !had_case {
                digs += 1;
            }
            had_case = now;
            assert_ne!(g.ds[feet] & 0x7f, 12, "the case is buried under the computer's feet");
        }
        assert!(digs <= 1, "the case went in and out of the hand {digs} times");
    }

    /// **stage 18f — button B is not button A.** `CS:0791` sends `0x3283` to `FUN_26c0_2d7e`, which
    /// throws; `0x327f` is the action key whose handler consumes it (`0x2d75`). The port's one key
    /// cannot stand for both on a computer spy: with B folded into A the AI buried and dug its own
    /// briefcase every other frame (2,373 frames at (6,160,48), STAGE18F_REPORT.md §3).
    #[test]
    fn button_b_throws_without_counting_as_an_action_tap() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(120), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        g.spies[0].blank = true;                // the throw's own gate (`CS:2f8a`)
        let b = Keys { left: true, fire_b: true, ..Keys::default() };
        fire_frames(&mut g, b, 1);
        assert!(g.spies[0].ball.is_some(), "B plus a direction is the throw");
        assert_eq!(g.spies[0].fire_taps, 0, "B must not count as an A tap");
        // and B alone does nothing: `0x2d7e` needs a direction flag (`0x2ff4`)
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(120), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        g.spies[0].blank = true;
        fire_frames(&mut g, Keys { fire_b: true, ..Keys::default() }, 2);
        assert!(g.spies[0].ball.is_none(), "B with no direction is not a throw");
    }

    /// **stage 18f — the human's one key is still a press, not a pulse.** The keyboard's flag is
    /// set on the key-down and consumed by whoever acts on it (`0x2d75`), so holding fire does not
    /// re-throw. This is the guard against the pulse leaking into the player's path.
    #[test]
    fn a_player_holding_fire_and_a_direction_does_not_punch_every_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(120), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        g.spies[0].blank = true;
        let held = Keys { left: true, fire: true, ..Keys::default() };
        let mut throws = 0usize;
        let mut busy = false;
        for _ in 0..60 {
            fire_frames(&mut g, held, 1);
            let now = g.spies[0].anim.map(|a| a.state) == Some(round::STATE_THROW);
            if now && !busy {
                throws += 1;
            }
            busy = now;
        }
        assert!(throws <= 1, "a held fire key is one press, not a pulse: {throws} throws");
    }

    /// Fire with nothing to pick up arms; the second tap opens the trapulator (`CS:2d50`).
    #[test]
    fn two_fire_taps_opens_the_trapulator() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        put_underfoot(&mut g, 0);
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.spies[0].fire_taps, 1);
        assert!(!g.spies[0].trap_open);
        fire_frames(&mut g, NOKEYS, 1);
        fire_frames(&mut g, FIRE, 1);
        assert!(g.spies[0].trap_open, "the second tap opens the panel");
    }

    /// A tool in hand and fire: the tool goes back into its slot — kit bit set, cursor moved,
    /// hand empty (`FUN_26c0_315f`, which is the routine `CS:07a0` calls while the latch is set).
    #[test]
    fn fire_puts_a_carried_tool_back_into_the_trapulator() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        put_underfoot(&mut g, 0);
        // the white bucket: island 1's kit (0x083A) has no bit for it (bits 0 and 2 are the two
        // tools the island lacks), which is the case M4c recorded being put away
        g.set_carried(0, 4);
        fire_frames(&mut g, FIRE, 1);       // press: latches
        assert_eq!(g.spies[0].carried, 4, "still in hand on the press frame");
        fire_frames(&mut g, NOKEYS, 1);     // release: put away
        assert_eq!(g.spies[0].carried, CARRY_NONE);
        assert_ne!(g.spies[0].kit & kit_bit(4), 0, "the bucket's slot bit is set");
        assert_eq!(g.spies[0].trap_cursor, items::code_cursor(&exe()[DS_FILE_OFF..], 4));
    }

    /// The trapulator itself: fire takes the selected slot's item into the hand and clears its
    /// bit (`FUN_26c0_2a4e`); **left/right walk the cursor** (`CS:2aed`/`CS:2b2c`, wrapping
    /// 0 <- 12) and **up/down close the panel** (`CS:2adc`).
    #[test]
    fn the_trapulator_gives_the_selected_slot_to_the_hand() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.spies[0].trap_open = true;
        g.spies[0].trap_cursor = 2;     // slot 1 = the saw (0x2D9A's word 2 = 3)
        g.spies[0].kit |= kit_bit(3);
        g.spies[0].fire_prev = false;
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.spies[0].carried, 3);
        assert_eq!(g.spies[0].kit & kit_bit(3), 0, "taking clears the slot bit");
        assert!(!g.spies[0].trap_open, "taking closes the panel");

        // the cursor walks with left/right and wraps at both ends
        let mut g = plain();
        g.spies[0].trap_open = true;
        g.spies[0].trap_cursor = 0;
        fire_frames(&mut g, LEFT, 1);
        assert_eq!(g.spies[0].trap_cursor, items::SLOT_MAP, "left from the detonator wraps to the map");
        fire_frames(&mut g, NOKEYS, 1);
        // the repeat counter is *not* reloaded on release, so the second step needs the
        // three-frame rhythm the code imposes (`CS:2af6`)
        fire_frames(&mut g, RIGHT, 3);
        assert_eq!(g.spies[0].trap_cursor, 0, "right from the map wraps back to the detonator");
        assert!(g.spies[0].trap_open, "moving the cursor keeps the panel open");
        fire_frames(&mut g, UP, 1);
        assert!(!g.spies[0].trap_open, "a vertical key closes the panel");
    }

    /// **read** (`CS:2aed`, `CS:2af6`) — the cursor's repeat delay `DS:0x2F9E`: a held key steps
    /// once and then once every third frame, and the counter is not reloaded on release.
    #[test]
    fn the_trapulators_cursor_repeats_every_third_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.spies[0].trap_open = true;
        g.spies[0].trap_cursor = 0;
        fire_frames(&mut g, RIGHT, 1);
        assert_eq!(g.spies[0].trap_cursor, 2, "the first frame moves at once");
        assert_eq!(g.spies[0].cursor_delay, 2);
        fire_frames(&mut g, RIGHT, 1);
        assert_eq!(g.spies[0].trap_cursor, 2, "then two frames of hold do nothing");
        assert_eq!(g.spies[0].cursor_delay, 1);
        fire_frames(&mut g, RIGHT, 1);
        assert_eq!(g.spies[0].cursor_delay, 0);
        fire_frames(&mut g, RIGHT, 1);
        assert_eq!(g.spies[0].trap_cursor, 4, "the third frame after a move steps again");
    }

    /// **read** (`CS:2a62`, and the disassembly of `2aa3`) — the slot -> code table is indexed by
    /// `cursor + player`, so each spy gets **its own** plunger and bucket. Stage 7's byte-offset
    /// read gave the black spy `0x0500` for the bucket: the trapulator light went out, the bucket
    /// vanished and the hand held nothing (the owner's report of 2026-10-24).
    #[test]
    fn each_spy_takes_its_own_tool_out_of_the_trapulator() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let img = &exe[DS_FILE_OFF..];
        // the executable's own table, read through both readings
        assert_eq!(items::slot_code(img, 0, 0), 1, "white's plunger");
        assert_eq!(items::slot_code(img, 0, 1), 2, "black's plunger");
        assert_eq!(items::slot_code(img, 4, 0), 4, "white's bucket");
        assert_eq!(items::slot_code(img, 4, 1), 5, "black's bucket");
        for cursor in (0..=12u16).step_by(2) {
            for player in 0..2usize {
                let code = items::slot_code(img, cursor, player);
                assert!(code <= 12, "cursor {cursor} player {player}: {code} is not an item code");
            }
        }
        // and the whole gesture, on the black spy: bucket in the slot, panel open, fire
        let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
        let bucket = 5u16;
        g.spies[1].kit |= kit_bit(bucket);
        g.spies[1].trap_open = true;
        g.spies[1].trap_cursor = 4;
        g.spies[1].fire_prev = false;
        let kit0 = g.spies[1].kit;
        for _ in 0..FRAME_TICKS {
            g.tick(Input { p1: Keys::default(), p2: FIRE });
        }
        assert_eq!(g.spies[1].carried, bucket, "black gets the black bucket, not 0x0500");
        assert_eq!(g.spies[1].kit, kit0 & !kit_bit(bucket), "and its dot goes dark");
        assert_eq!(g.spies[1].set, items::code_pose(img, bucket), "and the carrying pose is set");
    }

    /// The invariant behind the "second briefcase": with a corrupt carried word the pickup's
    /// `carried != CARRY_NONE` branch writes a **12** into the object block while the real case is
    /// somewhere else, so a level could hold two. No action may leave the hand outside 0..12.
    #[test]
    fn no_gesture_can_leave_the_carried_word_off_the_item_table() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for player in 0..2usize {
            for cursor in (0..=12u16).step_by(2) {
                let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                g.spies[player].kit = 0x83F;
                g.spies[player].trap_open = true;
                g.spies[player].trap_cursor = cursor;
                g.spies[player].fire_prev = false;
                for _ in 0..FRAME_TICKS {
                    let keys = Input {
                        p1: if player == 0 { FIRE } else { Keys::default() },
                        p2: if player == 1 { FIRE } else { Keys::default() },
                    };
                    g.tick(keys);
                }
                let c = g.spies[player].carried;
                assert!(c == CARRY_NONE || c <= 12, "cursor {cursor} player {player} left {c}");
            }
        }
    }

    /// `FUN_26c0_38e1` with `BX = 1`: fire held plus a direction buries the carried code, bit 7.
    /// A buried stick also takes the player's owner tag (0x20 white, 0x40 black) — `CS:396e`.
    #[test]
    fn fire_plus_a_direction_buries_the_carried_item() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let a = put_underfoot(&mut g, 0);
        g.set_carried(0, 12);
        fire_frames(&mut g, FIRE, 1);           // press: the latch
        fire_frames(&mut g, FIRE, 1);           // held: no direction, nothing happens yet
        assert_eq!(g.spies[0].carried, 12);
        g.tick(Input { p1: Keys { down: true, ..FIRE }, p2: Keys::default() });
        // one tick is not a frame; run the rest of the frame
        for _ in 1..FRAME_TICKS {
            g.tick(Input { p1: Keys { down: true, ..FIRE }, p2: Keys::default() });
        }
        assert_eq!(g.ds[a], 0x8C, "the briefcase, buried");
        assert_eq!(g.spies[0].carried, CARRY_NONE);

        let mut g = plain();
        let a = put_underfoot(&mut g, 0);
        g.set_carried(0, 8);
        g.spies[0].dyn_sticks = 0;
        fire_frames(&mut g, FIRE, 1);
        fire_frames(&mut g, Keys { down: true, ..FIRE }, 1);
        assert_eq!(g.ds[a], 0xA8, "dynamite: 0x20 (white's tag) | 0x80 (buried) | code 8");
    }

    /// A plain drop (fire, release, no direction) writes the bare code at the feet — the measured
    /// `code 12` of M4d.
    #[test]
    fn releasing_fire_drops_the_item_as_its_bare_code() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let a = put_underfoot(&mut g, 0);
        g.set_carried(0, 12);
        fire_frames(&mut g, FIRE, 1);
        fire_frames(&mut g, NOKEYS, 1);
        assert_eq!(g.ds[a], 12);
        assert_eq!(g.spies[0].carried, CARRY_NONE);
        assert_eq!(g.spies[0].kit & items::KIT_CASE_BIT, 0, "bit 10 follows the hand");
    }

    /// The briefcase in hand and a part under the feet: the part goes *into* the case — the
    /// deposited mask bit sets, the case stays in hand, the byte clears (`CS:3822`).
    #[test]
    fn the_briefcase_deposits_a_rocket_part() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let a = put_underfoot(&mut g, 0x89);
        g.spies[0].carried = 12;
        g.spies[0].case_owned = true;
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.ds[a], 0, "the part leaves the world");
        assert_eq!(g.spies[0].carried, 12, "the case stays in the hand");
        assert_eq!(g.mask, MASK_BASE | 0x40, "the deposited bit for part 9");
        assert_ne!(g.spies[0].kit & 0x40, 0);
    }

    /// The measured ledger is only set by a *deposit*: carrying a part does not set it (M4b/§1.3).
    #[test]
    fn carrying_a_part_does_not_touch_the_deposited_mask() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        put_underfoot(&mut g, 0x8A);
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.spies[0].carried, 10);
        assert_eq!(g.mask, MASK_BASE);
        assert_ne!(g.spies[0].kit & kit_bit(10), 0, "but the kit's mirror bit does set");
    }

    /// **The acceptance test**: every recorded *dig* in two sessions, replayed through the
    /// engine's own pickup routine. A pair qualifies when the before-dump has an empty hand and a
    /// `0x8x` byte at the spy's feet, and the after-dump has that byte cleared and holds its item
    /// code. The dump's blocks are lifted and the spy is seeded where the recording has him, then
    /// one fire press must reproduce the after-dump's byte *and* carried word *and* kit word.
    #[test]
    fn the_recorded_digs_replay_through_the_engine() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = crate::orig::require("recon/round/sessions");
        let mut pairs: Vec<(PathBuf, PathBuf, usize, u16)> = Vec::new();
        for ses in ["playtest5", "autoplay-pickup", "item-check-2"] {
            let mut ds: Vec<PathBuf> = std::fs::read_dir(base.join(ses))
                .map(|d| {
                    d.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("ds_")) == Some(true))
                        .collect()
                })
                .unwrap_or_default();
            ds.sort();
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
                if w16(&a, items::DS_COST_CARRIED) != CARRY_NONE {
                    continue;
                }
                let screen = w16(&a, 0x329F);
                let x = w16(&a, 0x329B) as i32;
                let y = w16(&a, 0x3297) as i32;
                let Some(cell) = items::cell_addr(&a, screen, x, y) else { continue };
                let byte = a[cell];
                let code = (byte & items::CODE_MASK) as u16;
                if byte & items::BURIED == 0 || code == 0 || code > 12 {
                    continue;
                }
                if b[cell] != 0 || w16(&b, items::DS_COST_CARRIED) != code {
                    continue;
                }
                pairs.push((w[0].clone(), w[1].clone(), cell, code));
            }
        }
        assert!(pairs.len() >= 8, "expected recorded digs, found {}", pairs.len());
        let mut ok = 0usize;
        for (before, after, cell, code) in pairs.iter().take(24) {
            let st = round::RoundState::from_ds_dump(before).unwrap();
            let mut g = plain();
            g.seed_from_dump(&st, true);
            assert_eq!(g.spies[0].carried, CARRY_NONE);
            fire_frames(&mut g, FIRE, 1);
            let after_ds = std::fs::read(after).unwrap();
            let after_kit = u16::from_le_bytes([
                after_ds[items::DS_COST_KIT], after_ds[items::DS_COST_KIT + 1]]);
            if g.spies[0].carried != *code || g.ds[*cell] != 0 || g.spies[0].kit != after_kit {
                panic!(
                    "{}: port gave carried {} cell {:#04x} kit {:#06x}; recording has {} / 0 / {:#06x}",
                    before.display(), g.spies[0].carried, g.ds[*cell], g.spies[0].kit,
                    code, after_kit);
            }
            ok += 1;
        }
        assert_eq!(ok, pairs.len().min(24));
        eprintln!("stage 7: {} recorded digs replayed, {} in this run", pairs.len(), ok);
    }

    /// **The drop and burial acceptance test**: every recorded pair where the before-dump carries
    /// something and the after-dump has it written into the world at the spy's feet (`code`, or
    /// `code | 0x80` for a burial). The engine must produce the same byte and the same empty hand.
    #[test]
    fn the_recorded_drops_replay_through_the_engine() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = crate::orig::require("recon/round/sessions");
        let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let mut pairs: Vec<(PathBuf, PathBuf, usize, u16, bool)> = Vec::new();
        for ses in ["playtest5", "trapulator-island4", "autoplay-pickup"] {
            let mut ds: Vec<PathBuf> = std::fs::read_dir(base.join(ses))
                .map(|d| {
                    d.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("ds_")) == Some(true))
                        .collect()
                })
                .unwrap_or_default();
            ds.sort();
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                let carried = w16(&a, items::DS_COST_CARRIED);
                if carried == CARRY_NONE || carried > 12 || w16(&b, items::DS_COST_CARRIED) != CARRY_NONE {
                    continue;
                }
                let screen = w16(&a, 0x329F);
                if screen >= FIRST_INTERIOR_SCREEN {
                    continue;
                }
                let x = w16(&a, 0x329B) as i32;
                let y = w16(&a, 0x3297) as i32;
                let Some(cell) = items::cell_addr(&a, screen, x, y) else { continue };
                if a[cell] != 0 {
                    // The original overwrote the byte already there in a handful of recorded
                    // drops (`trapulator-island4/ds_00087`: an ice patch under the feet, the part
                    // written over it). `CS:3924`'s search says it steps west first, so the port
                    // cannot reproduce that without inventing a branch; those pairs are counted
                    // and left out rather than quietly patched.
                    continue;
                }
                let got = b[cell];
                let want = carried as u8;
                if got & items::CODE_MASK == want & items::CODE_MASK
                    && (got == want || got == want | items::BURIED)
                {
                    pairs.push((w[0].clone(), w[1].clone(), cell, carried, got & items::BURIED != 0));
                }
            }
        }
        assert!(pairs.len() >= 6, "expected recorded drops, found {}", pairs.len());
        let mut ok = 0usize;
        for (before, after, cell, carried, buried) in pairs.iter() {
            let st = round::RoundState::from_ds_dump(before).unwrap();
            let mut g = plain();
            g.seed_from_dump(&st, true);
            // the recording's own hand; the seed reads it, but be explicit about the case bit
            g.spies[0].carried = *carried;
            g.spies[0].kit = st.players[0].spy.pose.rotate_left(0) as u16 * 0 + g.spies[0].kit;
            if *buried {
                fire_frames(&mut g, FIRE, 1);
                fire_frames(&mut g, Keys { down: true, ..FIRE }, 1);
            } else {
                fire_frames(&mut g, FIRE, 1);
                fire_frames(&mut g, NOKEYS, 1);
            }
            let after_ds = std::fs::read(after).unwrap();
            if g.ds[*cell] != after_ds[*cell] {
                panic!("{}: port wrote {:#04x} at {:#x}, recording has {:#04x}",
                       before.display(), g.ds[*cell], cell, after_ds[*cell]);
            }
            assert_eq!(g.spies[0].carried, CARRY_NONE, "{}: hand should be empty", before.display());
            ok += 1;
        }
        eprintln!("stage 7: {} recorded drops, {} replayed", pairs.len(), ok);
    }

    /// **The Trapulator acceptance test**, both directions, from the recordings.
    ///
    /// A *stow*: the before-dump carries a tool the kit word has no bit for, the after-dump has an
    /// empty hand, the bit set and the byte-block unchanged. A *take*: the before-dump has the
    /// panel open on a slot whose kit bit is set, the after-dump carries that slot's code with the
    /// bit cleared. The engine is seeded from the before-dump and driven with the one key the
    /// player would press.
    #[test]
    fn the_recorded_stows_and_takes_replay_through_the_engine() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = crate::orig::require("recon/round/sessions");
        let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let img = exe();
        let img = &img[DS_FILE_OFF..];
        let mut stows = 0usize;
        let mut takes = 0usize;
        let mut checked_stows = 0usize;
        let mut checked_takes = 0usize;
        for ses in ["trapulator-island4", "playtest5", "rocket-room-2"] {
            let mut ds: Vec<PathBuf> = std::fs::read_dir(base.join(ses))
                .map(|d| {
                    d.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("ds_")) == Some(true))
                        .collect()
                })
                .unwrap_or_default();
            ds.sort();
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                let (ca, cb) = (w16(&a, items::DS_COST_CARRIED), w16(&b, items::DS_COST_CARRIED));
                let (ka, kb) = (w16(&a, items::DS_COST_KIT), w16(&b, items::DS_COST_KIT));
                if ca == CARRY_NONE || cb != CARRY_NONE || ca > 12 {
                    // a take: nothing in hand -> a code, bit cleared
                    if ca == CARRY_NONE && cb != CARRY_NONE && cb <= 12 && w16(&a, items::DS_COST_OPEN) != 0 {
                        let cursor = w16(&a, items::DS_COST_CURSOR);
                        let bit = items::slot_bit(img, cursor);
                        if cursor != items::SLOT_MAP
                            && items::slot_code(img, cursor, 0) == cb
                            && ka & bit != 0
                            && kb & bit == 0
                        {
                            let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                            let mut g = plain();
                            g.seed_from_dump(&st, true);
                            fire_frames(&mut g, FIRE, 1);
                            if g.spies[0].carried != cb || g.spies[0].kit != kb {
                                panic!("{}: take gave carried {} kit {:#06x}; recording {} / {:#06x}",
                                       w[0].display(), g.spies[0].carried, g.spies[0].kit, cb, kb);
                            }
                            checked_takes += 1;
                        }
                        takes += 1;
                    }
                    continue;
                }
                let bit = items::code_bit(img, ca);
                if ca != items::CODE_DYNAMITE && ka & bit == 0 && kb & bit != 0 {
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = plain();
                    g.seed_from_dump(&st, true);
                    fire_frames(&mut g, FIRE, 1);
                    fire_frames(&mut g, NOKEYS, 1);
                    if g.spies[0].carried != CARRY_NONE || g.spies[0].kit != kb {
                        panic!("{}: stow of {} gave carried {} kit {:#06x}; recording none / {:#06x}",
                               w[0].display(), ca, g.spies[0].carried, g.spies[0].kit, kb);
                    }
                    checked_stows += 1;
                }
                stows += 1;
            }
        }
        eprintln!("stage 7: {stows} stow-shaped pairs ({checked_stows} replayed), \
{takes} take-shaped pairs ({checked_takes} replayed)");
        assert!(checked_stows >= 3, "no recorded stows closed the loop");
        assert!(checked_takes >= 1, "no recorded takes closed the loop");
    }

    /// The briefcase in hand over anything that is *not* a rocket part swaps: the case is put down
    /// in the item's place (`cell = 12`) and the item is taken (`CS:3827`).
    #[test]
    fn the_briefcase_swaps_with_a_tool() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let a = put_underfoot(&mut g, 0x04); // the white bucket, lying on the floor
        g.set_carried(0, 12);
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.ds[a], 12, "the case is left where the tool was");
        assert_eq!(g.spies[0].carried, 4, "and the tool is in the hand");
        assert_eq!(g.spies[0].kit & items::KIT_CASE_BIT, 0);
    }

    /// **recorded, both spies** — every take a recording shows out of the trapulator, for **either**
    /// spy, replayed through the engine.
    ///
    /// A take is a pair of dumps where the hand goes `-1 -> code` (1..8) and the kit word loses
    /// exactly that code's bit. The script presses the key the player pressed: the cursor is put on
    /// the code's own slot (`DS:0x2D7A`, the table the *stow* writes) because the dump's cursor is a
    /// 9.5 Hz sample of a field that moves at 10 Hz, then fire. The engine must hand back the code
    /// and the kit word the recording ended with.
    ///
    /// This is the test the stage-7 slot-table misreading failed: with `0x2D9A` read as
    /// `2*cursor + player` *bytes*, every black-spy take returned a value like 1536 and the pair
    /// could not replay. Measured on the corpus: 22 of the 71 recorded takes are the black spy's.
    #[test]
    fn every_recorded_trapulator_take_replays_for_both_spies() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let base = crate::orig::require("recon/round/sessions");
        let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let img = exe();
        let img = &img[DS_FILE_OFF..];
        let mut replayed = [0usize; 2];
        let mut shapes = [0usize; 2];
        for ses in ["trapulator-island4", "playtest5", "playtest4", "playtest2", "playtest1",
                    "playtest", "item-check-2", "rocket-room-2", "rocket-room-3-hard",
                    "autoplay-pickup"] {
            let mut ds: Vec<PathBuf> = std::fs::read_dir(base.join(ses))
                .map(|d| {
                    d.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("ds_")) == Some(true))
                        .collect()
                })
                .unwrap_or_default();
            ds.sort();
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for p in 0..2usize {
                    let o = 2 * p;
                    let (ca, cb) = (w16(&a, items::DS_COST_CARRIED + o),
                                    w16(&b, items::DS_COST_CARRIED + o));
                    let (ka, kb) = (w16(&a, items::DS_COST_KIT + o), w16(&b, items::DS_COST_KIT + o));
                    if ca != CARRY_NONE || cb == CARRY_NONE || cb > 8 {
                        continue;
                    }
                    let bit = items::code_bit(img, cb);
                    if ka & bit == 0 || kb & bit != 0 {
                        continue; // not a trapulator take (a pickup clears nothing)
                    }
                    shapes[p] += 1;
                    let cursor = items::code_cursor(img, cb);
                    if items::slot_code(img, cursor, p) != cb {
                        continue; // the other spy's tool: not takeable from this trapulator
                    }
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    g.spies[p].trap_open = true;
                    g.spies[p].trap_cursor = cursor;
                    g.spies[p].fire_prev = false;
                    let keys = Input {
                        p1: if p == 0 { FIRE } else { Keys::default() },
                        p2: if p == 1 { FIRE } else { Keys::default() },
                    };
                    for _ in 0..FRAME_TICKS {
                        g.tick(keys);
                    }
                    if g.spies[p].carried != cb || g.spies[p].kit != kb {
                        panic!("{} P{}: take of {} gave carried {} kit {:#06x}; recording {} / {:#06x}",
                               w[0].display(), p + 1, cb, g.spies[p].carried, g.spies[p].kit, cb, kb);
                    }
                    replayed[p] += 1;
                }
            }
        }
        eprintln!("stage 8: {} recorded takes replayed for the white spy, {} for the black spy \
({} / {} take-shaped pairs)", replayed[0], replayed[1], shapes[0], shapes[1]);
        assert!(replayed[0] >= 3, "no recorded white-spy take closed the loop");
        assert!(replayed[1] >= 3, "no recorded black-spy take closed the loop");
    }

    /// **stage 13b** — the detonator with nothing of mine under the opponent changes **no byte**
    /// but still plays its own four-frame state (0x0A) on the firer, keeps the plunger in the hand,
    /// and does not disturb the other spy. (Stage 13a's version of this test pinned the branch as
    /// unimplemented; this is the same boundary with the branch in.)
    #[test]
    fn the_plunger_on_empty_ice_changes_no_byte_but_plays_its_own_state() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let img = exe();
        let img = &img[DS_FILE_OFF..];
        for player in 0..2 {
            let code = items::slot_code(img, 0, player);
            let mut g = two_at(0, 4, 120, 44, 150);
            let a = put_underfoot_for(&mut g, player, 0);
            g.set_carried(player, code);
            let before = g.ds.clone();
            press_button_b(&mut g, player);
            assert_eq!(g.spies[player].carried, code, "the plunger stays in the hand");
            assert_eq!(g.ds[a], 0);
            let changed = before.iter().zip(g.ds.iter()).filter(|(x, y)| x != y).count();
            assert_eq!(changed, 0, "plunger {code}: no data-segment byte changed");
            assert_eq!(g.spies[player].anim.map(|s| s.state), Some(round::STATE_PLUNGER),
                       "the firer plays 0x0A");
            assert_eq!(g.spies[1 - player].anim, None, "the other spy is untouched");
            assert_eq!(g.spies[1 - player].act_state, 0);
        }
    }

    // ---- stage 13a: the three tools and the hazards they leave -------------------------------

    /// Two spies on one screen, the layout the hazard tests need (the visitor flag appears by
    /// itself on the second frame, through `update_visitor`).
    fn two_at(island: usize, screen: u16, x1: i32, y: i32, x2: i32) -> Game {
        let mut g = game(RoundOpts {
            island,
            players: 2,
            start_screen: Some(screen),
            start_x: Some(x1),
            start_x2: Some(x2),
            start_y: Some(y),
            ..RoundOpts::default()
        });
        g.players = 2;
        g
    }

    /// One game frame with per-player keys.
    fn frame2(g: &mut Game, p1: Keys, p2: Keys) {
        for _ in 0..FRAME_TICKS {
            g.tick(Input { p1, p2 });
        }
    }

    /// Fire pressed (button A latches the tool), then a direction while it is held (button B).
    fn press_button_b(g: &mut Game, p: usize) {
        let fire = Keys { fire: true, ..NOKEYS };
        let down = Keys { fire: true, down: true, ..NOKEYS };
        if p == 0 {
            frame2(g, fire, NOKEYS);
            frame2(g, down, NOKEYS);
        } else {
            frame2(g, NOKEYS, fire);
            frame2(g, NOKEYS, down);
        }
    }

    /// **The saw** — `CS:2e64..2ec4`: the hole lands one entry **in front** (the facing direction),
    /// twelve frames after the cut starts, and the tool stays in the hand. Both spies, both
    /// facings: the code's `0x2F14` test is the only thing that chooses west from east.
    #[test]
    fn the_saw_cuts_one_entry_in_front_for_both_spies_and_both_facings() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for p in 0..2 {
            for mirror in [false, true] {
                let mut g = two_at(0, 4, 120, 44, 150);
                g.spies[p].mirror = mirror;
                g.set_carried(p, items::CODE_SAW);
                let feet = g.cell_of(p).expect("feet");
                let target = if mirror { feet - 1 } else { feet + 1 };
                g.ds[target] = 0;
                let x0 = g.spies[p].x();
                let kit0 = g.spies[p].kit;
                press_button_b(&mut g, p);
                let Some(a) = g.spies[p].anim else {
                    panic!("P{} facing {}: the saw did not start", p + 1, if mirror { "W" } else { "E" });
                };
                assert_eq!(a.state, round::STATE_SAW, "0x32CB = 4");
                assert_eq!(a.cell, Some(target), "the cell is remembered at the start (0x331F)");
                assert_eq!(g.ds[target], 0, "nothing is written on the first frame");
                for _ in 0..11 {
                    frame2(&mut g, NOKEYS, NOKEYS);
                }
                assert_eq!(g.ds[target], 0, "still nothing on the twelfth animation frame");
                assert_eq!(g.spies[p].x(), x0, "and he has not moved a unit");
                frame2(&mut g, NOKEYS, NOKEYS);
                assert_eq!(g.ds[target], items::OBJ_HOLE,
                           "P{} facing {}: the hole is at {target:#06x}", p + 1,
                           if mirror { "W" } else { "E" });
                assert!(g.spies[p].anim.is_none(), "the animation ended");
                assert_eq!(g.spies[p].carried, items::CODE_SAW, "the saw is not consumed");
                assert_eq!(g.spies[p].kit, kit0, "and the kit is untouched");
                assert_eq!(g.item_counts[7], 1);
            }
        }
    }

    /// **The bucket** — the same walk, `0x0D` instead of `0x0E`, and the per-player code matters:
    /// the white spy's bucket is 4 and the black spy's 5 (`0x2DB6+si`).
    #[test]
    fn the_bucket_spills_one_entry_in_front_with_the_spies_own_bucket() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let img = exe();
        for p in 0..2 {
            let code = items::slot_code(&img[DS_FILE_OFF..], 4, p);
            assert_eq!(code, 4 + p as u16, "0x2DB6: white's bucket 4, black's 5");
            let mut g = two_at(0, 4, 120, 44, 150);
            g.set_carried(p, code);
            let feet = g.cell_of(p).expect("feet");
            let target = feet + 1; // facing east
            g.ds[target] = 0;
            press_button_b(&mut g, p);
            assert_eq!(g.spies[p].anim.map(|a| a.state), Some(round::STATE_BUCKET));
            for _ in 0..12 {
                frame2(&mut g, NOKEYS, NOKEYS);
            }
            assert_eq!(g.ds[target], items::OBJ_SPILL, "P{} spilled at {target:#06x}", p + 1);
            assert_eq!(g.spies[p].carried, code, "the bucket is not consumed");
        }
    }

    /// **The ice pick** — `CS:2f2b`: needs the **door arch** (terrain 0x0A) under the spy, writes
    /// three `0x14` bytes into **row 0** at `(x>>1)-1..+1` (no row term in the address) and sets
    /// the `0x2F6C` latch. The recorded placement is `iceaxe` ds_00323: island 3, screen 6,
    /// x 36, y 36 — the level-24 door plate at an even x.
    #[test]
    fn the_ice_pick_hangs_three_bytes_on_the_front_row() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for (p, x) in [(0usize, 36i32), (1usize, 38i32)] {
            let mut g = two_at(2, 6, 36, 36, 38);
            if p == 1 {
                g.spies[1].x3 = x * SUB;
            }
            // the round-start lottery's `2 -> 0x18` flip, which the `iceaxe` recording holds
            // (`round::place_igloo_fires`, `CS:26ae`): island 3's screen 6 cell 1 becomes level 24
            g.levels[3][1] = round::LEVEL_IGLOO_DOOR;
            assert_eq!(g.terrain.code(&g.levels, 6, x, 36), Some(terrain::T_DOOR_ARCH));
            assert_eq!(g.terrain.code(&g.levels, 6, 35, 36), Some(0), "and only at even x");
            g.set_carried(p, items::CODE_ICE_PICK);
            let base = round::block_addr(&exe()[DS_FILE_OFF..], 6).expect("block");
            let e = (x >> 1) as usize;
            press_button_b(&mut g, p);
            assert_eq!(g.spies[p].anim.map(|a| a.state), Some(round::STATE_ICE_PICK));
            assert_eq!(&g.ds[base + e - 1..base + e + 2], &[0x14, 0x14, 0x14][..],
                       "P{}: three bytes at entries {}..{} (x {})", p + 1, e - 1, e + 1, x);
            assert_eq!(g.ds[base + e - 2], 0, "and nothing one further west");
            assert_eq!(g.ds[base + e + 2], 0, "and nothing one further east");
            assert_eq!(&g.ds[base + 0x9C + e - 1..base + 0x9C + e + 2], &[0, 0, 0][..],
                       "nothing on the spy's own row");
            // the latch is set by `CS:2f5b` and then cleared by the same frame's hazard check,
            // because the placer's own feet cell (row 1) is empty — the recorded `iceaxe`
            // ds_00323 has `0x2F6C == 0` with the icicle already in row 0
            assert!(!g.spies[p].icicle_latch,
                    "the latch cannot survive the placer's empty feet cell");
            assert_eq!(g.spies[p].carried, items::CODE_ICE_PICK, "the pick is not consumed");
        }
    }

    /// The saw's two gates, from `CS:2e79` (the target must be empty) and `CS:2e90` (the terrain
    /// probe, taken at the **spy's own** cell, must be 0/5/0x10/0x11).
    #[test]
    fn the_saw_refuses_an_occupied_target_and_a_climb_underfoot() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // (a) the cell in front already holds something
        let mut g = two_at(0, 4, 120, 44, 150);
        g.set_carried(0, items::CODE_SAW);
        let feet = g.cell_of(0).unwrap();
        g.ds[feet + 1] = 8;
        press_button_b(&mut g, 0);
        assert!(g.spies[0].anim.is_none(), "no cut over an item");
        assert_eq!(g.ds[feet + 1], 8, "and the item is left alone");
        // (b) standing on a climb (island 1, screen 4, x 36, y 44 is terrain code 1)
        let mut g = two_at(0, 4, 36, 44, 60);
        assert_eq!(g.terrain.code(&g.levels, 4, 36, 44), Some(terrain::T_CLIMB));
        g.set_carried(0, items::CODE_SAW);
        press_button_b(&mut g, 0);
        assert!(g.spies[0].anim.is_none(), "no cut from a climb");
        // (c) the same spy on open ground does cut
        let mut g = two_at(0, 4, 120, 44, 150);
        g.set_carried(0, items::CODE_SAW);
        press_button_b(&mut g, 0);
        assert!(g.spies[0].anim.is_some());
    }

    /// The head of `FUN_26c0_2de3`: on a **climb** (`0x2FAA` = 1 or 0x17) no directed action runs —
    /// not the saw, not the bury. Island 1's screen 4, x 36, y 44 is terrain code 1.
    #[test]
    fn a_climb_refuses_even_the_bury() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_at(0, 4, 36, 44, 60);
        assert_eq!(g.terrain.code(&g.levels, 4, 36, 44), Some(terrain::T_CLIMB));
        g.set_carried(0, 9); // a rocket part: normally the direction press buries it
        let feet = g.cell_of(0).unwrap();
        press_button_b(&mut g, 0);
        assert_eq!(g.spies[0].carried, 9, "the part stays in the hand");
        assert_eq!(g.ds[feet], 0, "and nothing is written under him");
        // the same part on open ground does bury
        let mut g = two_at(0, 4, 120, 44, 150);
        g.set_carried(0, 9);
        let feet = g.cell_of(0).unwrap();
        press_button_b(&mut g, 0);
        assert_eq!(g.spies[0].carried, CARRY_NONE);
        assert_eq!(g.ds[feet], 9 | items::BURIED, "0x89 under the mound bit");
    }

    /// **The spill's victim** — `CS:2473`: the byte is consumed, the victim slips (state 0x0E for
    /// 40 frames), the other spy laughs (0x0C) and `FUN_26c0_315f` puts away what the victim holds.
    /// There is **no owner test**: the spy who spilled it slips just the same.
    #[test]
    fn a_spill_slips_whoever_stands_on_it_and_the_other_spy_laughs() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let img = exe();
        for victim in 0..2 {
            // the water bucket: island 1's kit lacks its bit, so `315f` *stows* it (a saw would be
            // dropped, because the island already has one — `playtest2` ds_00651 -> 652)
            let code = items::slot_code(&img[DS_FILE_OFF..], 4, victim);
            let mut g = two_at(0, 4, 120, 44, 150);
            let a = put_underfoot_for(&mut g, victim, items::OBJ_SPILL);
            g.set_carried(victim, code);
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.ds[a], 0, "the spill is consumed");
            assert_eq!(g.spies[victim].act_state, round::STATE_SLIP);
            assert_eq!(g.spies[victim].anim.map(|a| a.state), Some(round::STATE_SLIP));
            assert_eq!(g.spies[1 - victim].act_state, round::STATE_LAUGH,
                       "the other spy laughs (CS:2480)");
            assert_eq!(g.spies[1 - victim].anim.map(|a| a.state), Some(round::STATE_LAUGH));
            assert_eq!(g.spies[victim].carried, CARRY_NONE, "the bucket was put away");
            assert_ne!(g.spies[victim].kit & kit_bit(code), 0,
                       "into its trapulator slot, because its bit was clear");
            // the slip runs 40 frames (word[0x2CE6+0x0E] = 78 -> 78/2+1)
            for _ in 0..39 {
                frame2(&mut g, NOKEYS, NOKEYS);
            }
            assert!(g.spies[victim].anim.is_some(), "still slipping on frame 40");
            frame2(&mut g, NOKEYS, NOKEYS);
            assert!(g.spies[victim].anim.is_none(), "and free on frame 41");
            assert_eq!(g.spies[victim].y(), 44, "without leaving the cell");
        }
    }

    /// **The hole's victim** — `CS:24ac`: consumed, state 0x10 for 38 frames, and the put-away
    /// drops a *part* (whose kit bit is set) where a *tool* would be stowed.
    #[test]
    fn a_hole_swallows_the_victim_and_drops_a_part_on_the_ice() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for victim in 0..2 {
            let mut g = two_at(0, 4, 120, 44, 150);
            let a = put_underfoot_for(&mut g, victim, items::OBJ_HOLE);
            let x0 = g.spies[victim].x();
            g.set_carried(victim, 9);
            g.spies[victim].kit |= kit_bit(9);
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.spies[victim].anim.map(|a| a.state), Some(round::STATE_FALL));
            assert_eq!(g.spies[victim].carried, CARRY_NONE);
            assert_eq!(g.ds[a], 9,
                       "the hole is consumed and the part he held fell into the same cell");
            assert_eq!(g.spies[1 - victim].act_state, round::STATE_LAUGH);
            for _ in 0..38 {
                frame2(&mut g, NOKEYS, NOKEYS);
            }
            assert!(g.spies[victim].anim.is_none(), "38 frames (word[0x2CE6+0x10] = 74)");
            assert_eq!((g.spies[victim].x(), g.spies[victim].y()), (x0, 44),
                       "he climbs back out where he fell");
        }
    }

    /// **The icicle's victim** — `CS:24eb`: three bytes cleared from `feet-1`, the knock-out
    /// (state 0x1A for 36 frames), **y = 0x26** — he is dropped onto the next row — the host gets
    /// the `0x2F44` flag, and the victim's hands are *not* emptied (this branch has no `315f`).
    #[test]
    fn the_icicle_knocks_the_visitor_out_and_moves_him_to_row_one() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for victim in 0..2 {
            let mut g = two_at(0, 4, 120, 34, 150);
            let a = put_underfoot_for(&mut g, victim, items::OBJ_ICICLE);
            g.ds[a - 1] = items::OBJ_ICICLE;
            g.ds[a + 1] = items::OBJ_ICICLE;
            g.set_carried(victim, items::CODE_SAW);
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(&g.ds[a - 1..a + 2], &[0, 0, 0][..], "all three bytes cleared");
            assert_eq!(g.spies[victim].anim.map(|s| s.state), Some(round::STATE_KNOCKOUT));
            assert_eq!(g.spies[victim].y(), 38, "CS:2542 sets 0x3297 = 0x26");
            assert_eq!(g.spies[victim].carried, items::CODE_SAW, "he keeps what he holds");
            let host = if g.spies[victim].blank { 1 - victim } else { victim };
            assert!(g.spies[host].icicle_hit, "the host draws the icicle (0x2F44)");
            assert_eq!(g.spies[1 - victim].act_state, round::STATE_LAUGH);
            for _ in 0..35 {
                frame2(&mut g, NOKEYS, NOKEYS);
            }
            assert!(g.spies[victim].anim.is_some(), "36 frames (word[0x2CE6+0x1A] = 70)");
            frame2(&mut g, NOKEYS, NOKEYS);
            assert!(g.spies[victim].anim.is_none());
            assert!(!g.spies[host].icicle_hit, "0x2807 clears the flag");
        }
    }

    /// **The placer's immunity** — `0x2F6C`: standing in the **door's own row** (y 34, where the
    /// arch's mask is `0x0A` too) the icicle goes under the placer's feet, so the latch survives the
    /// same frame's hazard check and spares him. As soon as his feet cell is empty the latch clears,
    /// and the same icicle then knocks him out like anyone else.
    #[test]
    fn a_spy_is_spared_the_icicle_he_just_hung_until_he_steps_off() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_at(2, 6, 36, 34, 60);
        g.levels[3][1] = round::LEVEL_IGLOO_DOOR;
        assert_eq!(g.terrain.code(&g.levels, 6, 36, 34), Some(terrain::T_DOOR_ARCH),
                   "the arch's top row is 0x0A as well");
        g.set_carried(0, items::CODE_ICE_PICK);
        press_button_b(&mut g, 0);
        assert!(g.spies[0].icicle_latch, "he is standing on the icicle he just hung");
        assert_eq!(g.cell_of(0).map(|c| g.ds[c]), Some(items::OBJ_ICICLE));
        g.spies[0].anim = None;
        g.spies[0].pose_raised = false;
        frame2(&mut g, NOKEYS, NOKEYS);
        assert!(g.spies[0].anim.is_none(), "the latch holds: no knock-out");
        // step off (an empty cell) — the latch clears — and step back on
        let off = items::cell_addr(&g.ds, 6, 36, 44).expect("cell");
        g.ds[off] = 0;
        g.spies[0].y3 = 44 * SUB;
        frame2(&mut g, NOKEYS, NOKEYS);
        assert!(!g.spies[0].icicle_latch, "an empty feet cell clears 0x2F6C");
        g.spies[0].y3 = 34 * SUB;
        frame2(&mut g, NOKEYS, NOKEYS);
        assert_eq!(g.spies[0].anim.map(|s| s.state), Some(round::STATE_KNOCKOUT),
                   "the same icicle hits him now");
    }

    // ---- stage 13a: the recorded corpus -------------------------------------------------------

    fn session_dir() -> PathBuf {
        crate::orig::require("recon/round/sessions")
    }

    fn dumps_of(session: &str) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(session_dir().join(session))
            .map(|d| {
                d.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.file_name().map(|n| n.to_string_lossy().starts_with("ds_")) == Some(true)
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Every hazard byte in the five object blocks, as (block index, offset) -> byte. Block 4's
    /// row 0 is a live structure at `0x5E00`, not objects, so it is excluded.
    fn hazard_cells(d: &[u8]) -> Vec<((usize, usize), u8)> {
        let mut out = Vec::new();
        for (b, base) in [0x53BCusize, 0x562C, 0x589C, 0x5B0C, 0x5D7C].iter().enumerate() {
            let lo = if b == 4 { 0x9C } else { 0 };
            for off in lo..0x270 {
                if matches!(d[base + off], items::OBJ_SPILL | items::OBJ_HOLE | items::OBJ_ICICLE) {
                    out.push(((b, off), d[base + off]));
                }
            }
        }
        out
    }

    /// The object cell an object-block address names, as (block, offset).
    fn block_of(cell: usize) -> (usize, usize) {
        for (b, base) in [0x53BCusize, 0x562C, 0x589C, 0x5B0C, 0x5D7C].iter().enumerate() {
            if cell >= *base && cell < *base + 0x270 {
                return (b, cell - base);
            }
        }
        (usize::MAX, 0)
    }

    /// **The recorded writes** — every appearance of `0x0D`, `0x0E` or `0x14` in any session's
    /// object blocks, attributed with the code's own rule: the saw and the bucket write the cell one
    /// entry **in front** of the spy (`0x32D7 ± 1`, the sign from `0x2F14`), and the ice pick writes
    /// the three bytes of **row 0** around `x >> 1`.
    ///
    /// The actor is looked for in the birth dump and in the twelve dumps before it, because the
    /// write lands at the *end* of a 1.2 s animation and a slow dump ring can sample the spy only
    /// after he has walked on: his position during the animation is the position the cell was
    /// computed from. Measured: **23 of 23** (19 of them at the birth dump itself).
    #[test]
    fn every_recorded_hazard_write_matches_the_codes_cell_rule() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = [
            "autoplay-bucket-trap-2", "autoplay-bucket-trap-3", "iceaxe", "m1-fix-960",
            "m3-heat-90s", "playtest1", "playtest2", "playtest5", "rocket-room-2",
            "rocket-room-3-hard", "trapulator-island4",
        ];
        let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let mut born = 0usize;
        let mut at_birth = 0usize;
        let mut explained = 0usize;
        let mut misses: Vec<String> = Vec::new();
        for ses in sessions {
            let ds = dumps_of(ses);
            let mut prev: Option<Vec<((usize, usize), u8)>> = None;
            for (n, p) in ds.iter().enumerate() {
                let Ok(d) = std::fs::read(p) else { continue };
                if d.len() < 0x10000 {
                    continue;
                }
                let cur = hazard_cells(&d);
                if let Some(before) = &prev {
                    for (key, byte) in cur.iter() {
                        if before.iter().any(|(k, _)| k == key) {
                            continue;
                        }
                        born += 1;
                        let (b, off) = *key;
                        let mut found_here = false;
                        let mut found = false;
                        for j in n.saturating_sub(12)..=n {
                            let Ok(dj) = std::fs::read(&ds[j]) else { continue };
                            for i in 0..2usize {
                                let band = (w16(&dj, 0x329F + 2 * i) / 2) as usize;
                                let y = w16(&dj, 0x3297 + 2 * i) as i32;
                                let x = w16(&dj, 0x329B + 2 * i) as i32;
                                if band != b || !(0..4).contains(&((y - 28) >> 3)) {
                                    continue;
                                }
                                let feet = [0x000usize, 0x09C, 0x138, 0x1D4][((y - 28) >> 3) as usize]
                                    + (x >> 1) as usize;
                                let hit = if *byte == items::OBJ_ICICLE {
                                    // row 0 only, and only around the spy's own entry
                                    off < 0x9C && (off as i32 - (x >> 1)).abs() <= 1
                                } else {
                                    let want = if w16(&dj, 0x2F14 + 2 * i) != 0 {
                                        feet as i32 - 1
                                    } else {
                                        feet as i32 + 1
                                    };
                                    want == off as i32
                                };
                                if hit {
                                    found = true;
                                    if j == n {
                                        found_here = true;
                                    }
                                }
                            }
                        }
                        if found {
                            explained += 1;
                        } else {
                            misses.push(format!("{}/{} band {} offset {:#05x} byte {:#04x}",
                                                 ses, p.file_name().unwrap().to_string_lossy(),
                                                 b, off, byte));
                        }
                        if found_here {
                            at_birth += 1;
                        }
                    }
                }
                prev = Some(cur);
            }
        }
        for m in &misses {
            eprintln!("stage 13a: no recorded cell rule explains {m}");
        }
        eprintln!("stage 13a: {born} recorded hazard writes, {explained} explained by the code's rule ({at_birth} at the birth dump)");
        assert!(born >= 20, "expected the corpus's hazard writes, found {born}");
        assert_eq!(explained, born, "every recorded write must match the rule");
    }

    /// **The recorded bites** — every recorded *consumption* of a hazard byte: the victim is
    /// standing on it in the before dump and it is gone in the after dump. The engine is seeded
    /// from the before dump (blocks lifted, both spies' positions and item words) and given one
    /// frame with no keys; it must reproduce the after dump: the cleared byte, the state the
    /// hazard starts, the knock-out's `y = 0x26`, the victim's hand and kit, and the other spy's
    /// laugh.
    #[test]
    fn every_recorded_hazard_bite_replays_byte_for_byte() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = [
            "iceaxe", "m3-heat-90s", "playtest1", "playtest2", "playtest5", "rocket-room-2",
            "rocket-room-3-hard", "trapulator-island4",
        ];
        let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let mut tried = 0usize;
        let mut ok = 0usize;
        let mut kinds = [0usize; 3];
        for ses in sessions {
            let ds = dumps_of(ses);
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for victim in 0..2usize {
                    let band = w16(&a, 0x329F + 2 * victim);
                    let x = w16(&a, 0x329B + 2 * victim) as i32;
                    let y = w16(&a, 0x3297 + 2 * victim) as i32;
                    if w16(&a, 0x2F70 + 2 * victim) != 0 || w16(&a, 0x2F64 + 2 * victim) >= 2 {
                        continue;
                    }
                    let Some(cell) = items::cell_addr(&a, band, x, y) else { continue };
                    let byte = a[cell];
                    if !matches!(byte, items::OBJ_SPILL | items::OBJ_HOLE | items::OBJ_ICICLE) {
                        continue;
                    }
                    // the hazard must be *gone* in the after dump at this cell, and the victim must
                    // not have moved (he cannot while the animation runs)
                    if b[cell] == byte {
                        continue;
                    }
                    let kind = match byte {
                        items::OBJ_SPILL => round::STATE_SLIP,
                        items::OBJ_HOLE => round::STATE_FALL,
                        _ => round::STATE_KNOCKOUT,
                    };
                    kinds[match byte {
                        items::OBJ_SPILL => 0,
                        items::OBJ_HOLE => 1,
                        _ => 2,
                    }] += 1;
                    tried += 1;
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    // rewind any animation the dump caught: the bite happens on the first frame
                    for i in 0..2 {
                        g.spies[i].anim = None;
                        g.spies[i].pose_raised = false;
                    }
                    g.hatch.other = None;
                    frame2(&mut g, NOKEYS, NOKEYS);
                    let mut bad = Vec::new();
                    if g.spies[victim].anim.map(|a| a.state) != Some(kind) {
                        bad.push(format!("state {:?} not {:?}", g.spies[victim].anim, kind));
                    }
                    if byte == items::OBJ_ICICLE {
                        // three bytes from feet-1
                        for k in 0..3usize {
                            let c = cell - 1 + k;
                            if g.ds[c] != 0 {
                                bad.push(format!("icicle byte {c:#06x} is {:#04x}", g.ds[c]));
                            }
                        }
                        if g.spies[victim].y() != 0x26 {
                            bad.push(format!("y {} not 38", g.spies[victim].y()));
                        }
                    } else if g.ds[cell] != b[cell] {
                        bad.push(format!("cell {cell:#06x} is {:#04x}, recording has {:#04x}",
                                         g.ds[cell], b[cell]));
                    }
                    let other = 1 - victim;
                    if g.spies[other].act_state != round::STATE_LAUGH {
                        bad.push(format!("other state {:#04x} not 0x0C", g.spies[other].act_state));
                    }
                    if g.spies[victim].carried != w16(&b, items::DS_COST_CARRIED + 2 * victim) {
                        bad.push(format!("carried {} not {}", g.spies[victim].carried,
                                         w16(&b, items::DS_COST_CARRIED + 2 * victim)));
                    }
                    if g.spies[victim].kit != w16(&b, items::DS_COST_KIT + 2 * victim) {
                        bad.push(format!("kit {:#06x} not {:#06x}", g.spies[victim].kit,
                                         w16(&b, items::DS_COST_KIT + 2 * victim)));
                    }
                    if bad.is_empty() {
                        ok += 1;
                    } else {
                        panic!("{}/{} P{} byte {:#04x}: {}", ses,
                               w[0].file_name().unwrap().to_string_lossy(), victim + 1, byte,
                               bad.join("; "));
                    }
                }
            }
        }
        eprintln!("stage 13a: {ok} of {tried} recorded hazard bites replayed (spill {}, hole {}, icicle {})", kinds[0], kinds[1], kinds[2]);
        assert!(tried >= 8, "expected the corpus's hazard bites, found {tried}");
        assert_eq!(ok, tried);
        assert!(kinds[0] >= 2 && kinds[1] >= 3 && kinds[2] >= 1,
                "each hazard must appear: {kinds:?}");
    }

    /// The snowball's gates are the code's: both spies must be on one screen (`0x2F7E`/`0x2F80`),
    /// and with nothing in hand a direction press consumes the frame and sets action state 2.
    #[test]
    fn the_snowball_needs_both_spies_on_one_screen() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.players = 2;
        put_underfoot(&mut g, 0);
        // **stage 22c** — pin the two spies' geometry. The round start is the original's random
        // draw (and a one-player round now draws *both* spies, as `CS:257b` does), so a test of the
        // throw's gates must not inherit its numbers from the layout: host (spy 2) on screen 4, his
        // view at 80, and the visitor 20 units east of that view — inside `map_x - 4 < x < map_x +
        // 0x29`, where the flag survives `update_visitor`.
        for p in 0..2 {
            g.spies[p].y3 = 44 * SUB;
        }
        g.spies[1].screen = 4;
        g.spies[1].map_x = 80;
        g.spies[1].x3 = 90 * SUB;
        g.spies[0].x3 = 100 * SUB;
        // apart: nothing happens
        g.spies[0].screen = 6;
        fire_frames(&mut g, FIRE, 1);
        fire_frames(&mut g, Keys { right: true, ..FIRE }, 1);
        assert_eq!(g.spies[0].act_state, 0, "no throw across two screens");
        // together, and inside the shared window so the visitor flag survives `update_visitor`
        g.spies[0].screen = 4;
        g.spies[0].x3 = 100 * SUB;
        g.spies[0].blank = true; // the visitor flag, as `FUN_26c0_0000` sets it
        g.spies[0].fire_taps = 0; // otherwise the earlier tap opens the trapulator
        fire_frames(&mut g, NOKEYS, 1);
        fire_frames(&mut g, FIRE, 1);
        fire_frames(&mut g, Keys { right: true, ..FIRE }, 1);
        assert_eq!(g.spies[0].act_state, 2, "0x2F87 sets action state 2");
    }

    // ---- stage 16b: the snowball (`FUN_26c0_2f87`, `CS:303c`, `CS:30c9`) ----------------------

    /// Clear the cells a fire tap can pick out of, so the button-B gesture reaches the throw: with
    /// **one** fire key the press runs the button-A tap first (`CS:0782` before `CS:0791`), and a tap
    /// standing on an item takes it — which is the stage-7 deviation showing through, not a bug in
    /// the throw (see STAGE16B_REPORT.md §4.3).
    fn clear_pickups(g: &mut Game, p: usize) {
        if let Some(feet) = g.cell_of(p) {
            let lo = feet.saturating_sub(3);
            let hi = (feet + 4).min(g.ds.len());
            for k in lo..hi {
                g.ds[k] = 0;
            }
        }
    }

    /// One button-B press: fire on one frame, then fire + a direction on the next (the port's one
    /// key; the original's two buttons do it in one, `CS:0782`/`CS:0791`).
    fn throw_frame(g: &mut Game, p: usize, dir: Keys) {
        let fire = Keys { fire: true, ..NOKEYS };
        let both = Keys { fire: true, ..dir };
        frame_p(g, p, fire);
        frame_p(g, p, both);
    }

    /// One game frame with `k` in player `p`'s hands.
    fn frame_p(g: &mut Game, p: usize, k: Keys) {
        if p == 0 {
            frame2(g, k, NOKEYS);
        } else {
            frame2(g, NOKEYS, k);
        }
    }

    /// **The tables.** `FUN_26c0_277c` reads the pose set from `0x2CC4` and the frame ceiling from
    /// `0x2CE6`, both with the state as a byte offset: the throw is state 2, pose set `0x88`,
    /// ceiling 14 (**eight** frames), and the two knock-outs it causes are states `0x16`/`0x18`
    /// with pose sets `0x2AC`/`0x2B0` and ceiling 2 (**two** frames each) — a hit reaction, not the
    /// icicle's 36-frame knock-out (`0x1A`). The ball's own record at `DS:0x6F66` is 6 rows x 2
    /// bytes, mask plane then data plane.
    #[test]
    fn the_throw_and_its_two_knock_out_tables_are_the_executables() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let img = &exe[DS_FILE_OFF..];
        assert_eq!(round::STATE_THROW, 0x02);
        assert_eq!(round::anim_set(img, round::STATE_THROW), 0x88);
        assert_eq!(round::state_ceiling(img, round::STATE_THROW), 14);
        assert_eq!(14 / 2 + 1, 8, "eight drawn frames");
        assert_eq!(round::STATE_KNOCK_FACE, 0x16);
        assert_eq!(round::STATE_KNOCK_BACK, 0x18);
        assert_eq!(round::anim_set(img, 0x16), 0x2AC);
        assert_eq!(round::anim_set(img, 0x18), 0x2B0);
        assert_eq!(round::state_ceiling(img, 0x16), 2);
        assert_eq!(round::state_ceiling(img, 0x18), 2);
        assert_eq!(round::BALL_RECORD, 0x6F66);
        let rec = round::Record::expand_planes(&img[round::BALL_RECORD..]).expect("the ball record");
        assert_eq!((rec.height, rec.width), (6, 2), "6 rows, 2 bytes (8 px) wide");
        // the ball is drawn on a fixed row 0x12 above the thrower's feet, moving 3 world bytes a
        // frame, and the thrower cannot be standing on a climb, deep snow, ice or high water
        assert_eq!((BALL_ROW_FROM_FEET, BALL_SPEED, BALL_EAST_LEAD), (0x12, 3, 6));
        assert_eq!((BALL_ROW_MAX, BALL_VIEW_MARGIN, BALL_VIEW_WIDTH), (0x35, 2, 0x2C));
        assert_eq!(BALL_REFUSED_TERRAIN, [1u16, 2, 3, 0x17]);
    }

    /// **The gates** (`CS:2d7e` then `CS:2f87`): both spies on one screen (the two visitor flags),
    /// the other spy still in the round, no ball of mine already flying, and a left/right press to
    /// pick the direction. A tool in the hand is *not* a throw, and the throw costs no item, no
    /// snow and no heat.
    #[test]
    fn the_snowball_gates_are_the_codes() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // apart: no throw
        let mut g = two_at(0, 4, 120, 44, 150);
        g.spies[1].screen = 6;
        g.spies[0].blank = false;
        g.spies[1].blank = false;
        throw_frame(&mut g, 0, RIGHT);
        assert_eq!(g.spies[0].anim.map(|a| a.state), None, "no throw across two screens");
        assert_eq!(g.spies[0].ball.map(|b| b.flying), None);

        // one screen: the visitor flag is set by `update_visitor`, and the throw happens
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        assert!(g.spies[0].blank || g.spies[1].blank, "the two spies are on one screen");
        throw_frame(&mut g, 0, RIGHT);
        assert_eq!(g.spies[0].anim.map(|a| a.state), Some(round::STATE_THROW));

        // the other spy out of the round: refused (`0x2F64 >= 1`)
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        g.spies[1].frozen = 1;
        g.spies[1].out_of_round = true;
        throw_frame(&mut g, 0, RIGHT);
        assert_eq!(g.spies[0].anim, None, "a frozen visitor is not a target");

        // a ball already flying: refused (`0x2F40`)
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        // a ball of mine that is alive and missing him: it blocks the second throw
        g.spies[0].ball = Some(Ball { row: 26, col: 135, vx: 3, flying: true });
        throw_frame(&mut g, 0, RIGHT);
        assert_eq!(g.spies[0].anim, None, "one ball at a time");

        // a tool in the hand: the bury, not a throw
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        let feet = g.cell_of(0).unwrap();
        g.ds[feet] = 0;
        g.set_carried(0, items::CODE_SAW);
        throw_frame(&mut g, 0, RIGHT);
        assert_eq!(g.spies[0].anim.map(|a| a.state), Some(round::STATE_SAW));

        // the ground the thrower stands on: `CS:2fa4..2fbe` refuses a climb (1), deep snow (2),
        // ice (3) and high water (0x17). Island 1 has such cells; find one and try it for real.
        let mut g = two_at(0, 4, 120, 44, 126);
        let found = refused_cell(&g).expect("island 1 has a cell the throw refuses");
        let (screen, x, y, code) = found;
        g.spies[0].screen = screen;
        g.spies[0].x3 = x * SUB;
        g.spies[0].y3 = y * SUB;
        g.spies[1].screen = screen;
        g.spies[1].x3 = (x + 6) * SUB;
        g.spies[1].y3 = y * SUB;
        g.spies[0].blank = false;
        g.spies[1].blank = false;
        g.spies[0].map_x = (x - MAP_X_ON_ENTRY_OFFSET).clamp(0, MAP_X_MAX);
        g.spies[1].map_x = (x + 6 - MAP_X_ON_ENTRY_OFFSET).clamp(0, MAP_X_MAX);
        frame2(&mut g, NOKEYS, NOKEYS);
        assert!(g.spies[0].blank || g.spies[1].blank, "on one screen at {screen}/{x}/{y}");
        throw_frame(&mut g, 0, RIGHT);
        assert_eq!(g.spies[0].anim, None, "terrain {code:#04x} refuses the throw");
    }

    /// A cell on island 1 whose terrain refuses a snowball (`1`, `2`, `3` or `0x17`), with room for
    /// the other spy six bytes east of it.
    fn refused_cell(g: &Game) -> Option<(u16, i32, i32, u16)> {
        // deep snow (2) and ice (3) first: water (1) and the high-water climb (0x17) sink a spy who
        // stands on them, which would move the cell under his feet during the frame
        for want in [[2u16, 3], [1, 0x17]] {
            for screen in [0u16, 2, 4, 6, 8] {
                for y in (34..=58).step_by(2) {
                    for x in 8..=264 {
                        if let Some(c) = g.terrain.code(&g.levels, screen, x, y) {
                            if want.contains(&c) {
                                return Some((screen, x, y, c));
                            }
                        }
                    }
                }
            }
        }
        None
    }


    /// **The launch frame.** `CS:09b1` sets `0x2F40` when the throw's animation reaches frame 8
    /// (`0x32CF == 2 && 0x2EFC == 8`), and `CS:303c` then steps and draws the ball in the *same*
    /// frame — so the first position ever drawn is the launch column plus one velocity. The row is
    /// `y - 0x12`; the column is `x + 6` for an eastward throw and `x` for a westward one, with the
    /// velocity ±3; the facing (`0x2F14`) is set by the throw itself.
    #[test]
    fn a_throw_puts_the_ball_on_the_ice_on_its_eighth_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for (dir, mirror, lead, vx) in [(RIGHT, false, 6, 3), (LEFT, true, 0, -3)] {
            // the other spy is on the next row, outside the ball's 11-row box, so the flight is
            // the only thing this test measures
            let mut g = two_at(0, 4, 120, 44, 126);
            frame2(&mut g, NOKEYS, NOKEYS);
            g.spies[1].y3 = 54 * SUB;
            clear_pickups(&mut g, 0);
            let y = g.spies[0].y();
            let x = g.spies[0].x();
            throw_frame(&mut g, 0, dir);
            assert_eq!(g.spies[0].anim.map(|a| a.state), Some(round::STATE_THROW));
            assert_eq!(g.spies[0].act_state, round::STATE_THROW);
            assert_eq!(g.spies[0].mirror, mirror, "the throw sets the facing");
            let b = g.spies[0].ball.expect("the ball's words are written at the throw");
            assert_eq!((b.row, b.col, b.vx), (y - BALL_ROW_FROM_FEET, x + lead, vx));
            // the throw costs nothing and clears the tap counter (`CS:3035`)
            assert_eq!(g.spies[0].fire_taps, 0, "`0x2F34` back to zero");
            assert_eq!(g.spies[0].heat, 3, "no heat");
            assert_eq!(g.spies[0].carried, CARRY_NONE, "no item");
            assert_eq!(g.spies[0].kit, 0x83A, "no kit word");
            assert!(!b.flying, "0x2F40 is still clear on the throw's first frame");
            assert_eq!(g.spies[0].pose_state, round::STATE_THROW);
            assert_eq!(g.spies[0].set, 0x88);
            // frames 2, 4, 6 are the wind-up and 8 is the frame the test at `CS:09b1` reads; the
            // release is the frame *after* it, the one that stores 10 (`0xa2e` runs after `9b1`)
            for want in [2u16, 4, 6, 8] {
                frame2(&mut g, NOKEYS, NOKEYS);
                assert!(!g.spies[0].ball.unwrap().flying);
                assert_eq!(g.spies[0].frame, want, "`0x2EFC` = {want}");
            }
            assert_eq!(g.spies[0].frame, BALL_LAUNCH_FRAME as u16,
                       "`0x2EFC = 8` with `0x2F40 = 0`, as playtest5 ds_00820 holds it");
            frame2(&mut g, NOKEYS, NOKEYS); // frame 8 -> 10: the release
            let b = g.spies[0].ball.expect("the ball");
            assert!(b.flying, "the ball leaves on the frame that reads 8");
            assert_eq!(b.col, x + lead + vx, "and it has already stepped once");
        }
    }

    /// **The flight** (`CS:303e..30a7`): the row never changes, the column steps `0x32FB` a frame,
    /// and the ball dies when the column leaves the thrower's window — `[view - 2, view + 0x2C)`.
    /// With nobody in the way it crosses the room and vanishes at the far edge.
    #[test]
    fn the_ball_flies_three_bytes_a_frame_and_dies_at_the_view_edge() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // an eastward ball with the other spy a row down, outside the 11-row box: it crosses the
        // room and dies at the right edge
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        g.spies[1].y3 = 54 * SUB;
        clear_pickups(&mut g, 0);
        throw_frame(&mut g, 0, RIGHT);
        for _ in 0..5 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        let mut cols = vec![g.spies[0].ball.unwrap().col];
        assert!(g.spies[0].ball.unwrap().flying);
        let view = g.spies[0].map_x;
        let mut steps = 0;
        loop {
            frame2(&mut g, NOKEYS, NOKEYS);
            match g.spies[0].ball {
                Some(b) if b.flying => {
                    assert_eq!(b.col, *cols.last().unwrap() + 3, "3 world bytes a frame");
                    assert_eq!(b.row, 44 - BALL_ROW_FROM_FEET, "the row is fixed");
                    cols.push(b.col);
                    steps += 1;
                    assert!(steps < 40, "the ball never left the window: {cols:?}");
                }
                _ => break,
            }
        }
        let last = *cols.last().unwrap();
        assert!(last >= view + BALL_VIEW_WIDTH || last + 3 >= view + BALL_VIEW_WIDTH,
                "died at the far edge: last {last}, view {view}, cols {cols:?}");
        assert!(cols.len() >= 5, "the window is 44 bytes and the ball starts 28 in: {cols:?}");
        assert!(g.spies[0].ball.map(|b| b.flying) != Some(true));
    }

    /// **The hit test and the knock-out** (`CS:3087..312d`): the column must be in
    /// `[his_x + 1, his_x + 4]` and the row in `[his_y - 0x18, his_y - 0x0E]`; the ball is then
    /// consumed, and the victim takes `0x16` when the ball meets his face and `0x18` when it takes
    /// him from behind (`0x2F14 + 0x32FB`). He drops nothing, keeps what he holds, and loses no
    /// heat: the branch reads `0x32CF`, `0x2F40` and `0x32CB` only.
    #[test]
    fn a_ball_knocks_the_other_spy_out_in_the_face_or_from_behind() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // (thrower's direction, thrower x, victim x, victim's facing, expected state): a ball
        // travelling the way the victim faces takes him from behind (0x18), one travelling into
        // his face is 0x16.
        let cases = [
            (RIGHT, 120, 126, false, round::STATE_KNOCK_BACK),
            (RIGHT, 120, 126, true, round::STATE_KNOCK_FACE),
            (LEFT, 126, 120, false, round::STATE_KNOCK_FACE),
            (LEFT, 126, 120, true, round::STATE_KNOCK_BACK),
        ];
        for (dir, tx, vx_, vmirror, expect) in cases {
            for thrower in 0..2usize {
                let victim = 1 - thrower;
                let (x1, x2) = if thrower == 0 { (tx, vx_) } else { (vx_, tx) };
                let mut g = two_at(0, 4, x1, 44, x2);
                frame2(&mut g, NOKEYS, NOKEYS);
                clear_pickups(&mut g, thrower);
                g.spies[victim].mirror = vmirror;
                let before_carried = g.spies[victim].carried;
                let before_kit = g.spies[victim].kit;
                let before_heat = g.spies[victim].heat;
                let snapshot = g.ds.clone();
                throw_frame(&mut g, thrower, dir);
                for _ in 0..5 {
                    frame2(&mut g, NOKEYS, NOKEYS);
                }
                assert_eq!(g.spies[victim].act_state, expect,
                           "thrower {thrower} dir {} victim facing west {vmirror}",
                           if dir == RIGHT { "east" } else { "west" });
                assert_eq!(g.spies[victim].anim.map(|a| a.state), Some(expect));
                // **stage 24d** — the pass is per player (`order` is [1, 0], so player 2's body
                // and ball come first). A ball of player 2's therefore lands *before* player 1's
                // body and his animation takes its first step in the frame it lands (`0x2EFC`
                // -2 -> 0); a ball of player 1's lands after player 2's body, so the victim is
                // still on the seed frame. The recording agrees: `every_recorded_snowball_knock_
                // out_replays_byte_for_byte` sees both values.
                let seed = if thrower == 1 { 0 } else { -2 };
                assert_eq!(g.spies[victim].anim.map(|a| a.frame), Some(seed),
                           "the victim of player {}'s ball, whose pass is {seed} in this frame",
                           thrower + 1);
                assert_eq!(g.spies[victim].set, round::anim_set(&exe()[DS_FILE_OFF..], expect));
                assert_eq!(g.spies[thrower].ball.map(|b| b.flying), Some(false),
                           "the ball is consumed by the hit (`0x2F40` cleared, the words kept)");
                assert_eq!(g.spies[victim].carried, before_carried, "nothing is put away");
                assert_eq!(g.spies[victim].kit, before_kit, "nothing moves in the kit");
                assert_eq!(g.spies[victim].heat, before_heat + 4,
                           "the hit costs 4 cold (`CS:30ee`, two steps) since stage 20a");
                let changed: Vec<usize> = (0..snapshot.len())
                    .filter(|&i| snapshot[i] != g.ds[i])
                    .collect();
                assert!(changed.is_empty(), "no object byte moves: {changed:?}");
            }
        }
    }

    /// The box is what the code says it is, and a miss is a miss: one row up (y - 6) is outside
    /// `[y - 0x18, y - 0x0E]`, three rows down (y + 6) is still inside, and a victim standing at
    /// the thrower's own column is never hit — the first column the ball tests is already 9 bytes
    /// past him.
    #[test]
    fn a_ball_outside_the_box_flies_on() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // one row up: the ball passes over him
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        clear_pickups(&mut g, 0);
        g.spies[1].y3 = (44 - 6) * SUB;
        throw_frame(&mut g, 0, RIGHT);
        for _ in 0..6 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        assert_eq!(g.spies[1].act_state, 0, "6 rows up is above the box");

        // three rows down: still inside (the box is 11 rows tall)
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        clear_pickups(&mut g, 0);
        g.spies[1].y3 = (44 + 6) * SUB;
        throw_frame(&mut g, 0, RIGHT);
        for _ in 0..5 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        assert_eq!(g.spies[1].act_state, round::STATE_KNOCK_BACK, "6 rows down is inside");

        // the victim at the thrower's own column: the first tested column is x + 9
        let mut g = two_at(0, 4, 120, 44, 120);
        frame2(&mut g, NOKEYS, NOKEYS);
        clear_pickups(&mut g, 0);
        throw_frame(&mut g, 0, RIGHT);
        for _ in 0..6 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        assert_eq!(g.spies[1].act_state, 0, "a spy on the thrower's column is never hit");
    }

    /// **Who can be hit** (`CS:30aa..30c6`): not a climber (`0x2F68`), not a spy indoors
    /// (`0x329F >= 0xA`), and not one whose animation is `0x0C` or above — while a spy who is
    /// *throwing* (state 2) can be hit, and `FUN_26c0_277c` lets `0x16`/`0x18` take the animation
    /// over from any state at or below 2.
    ///
    /// The three refusals are asserted against the test itself rather than through a ball, because
    /// `0x2F68` and the animation's state are words the frame body rewrites every frame: forcing
    /// them and then flying a ball for four frames would be testing the forcing, not the gate.
    #[test]
    fn a_climbing_or_flinching_spy_is_immune_but_a_thrower_is_not() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let ball = Ball { row: 26, col: 129, vx: 3, flying: true };
        // the box itself: his x 126, y 44 -> column [127,130], row [20,30]
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        assert!(g.snowball_hits(0, 1, ball), "a standing spy in the box is hit");

        g.spies[1].pose_clip = 6; // a climber's `0x2F68`
        assert!(!g.snowball_hits(0, 1, ball), "a climbing spy ducks the ball");
        g.spies[1].pose_clip = 0;

        g.spies[1].screen = 10; // indoors
        assert!(!g.snowball_hits(0, 1, ball), "an indoor spy is not hit");
        g.spies[1].screen = 4;

        // a running state: `0x2F18` set and `0x32CF >= 0x0C` refuses, 2 does not
        g.start_anim(1, round::STATE_SLIP, None);
        assert!(g.spies[1].pose_raised);
        assert_eq!(g.spies[1].pose_state, round::STATE_SLIP);
        assert!(!g.snowball_hits(0, 1, ball), "a slipping spy is not hit");

        // ... and the end-to-end case: a spy who is *throwing* can be knocked out, because
        // `FUN_26c0_277c` lets 0x16/0x18 take over from any state at or below 2
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        clear_pickups(&mut g, 0);
        g.start_anim(1, round::STATE_THROW, None);
        throw_frame(&mut g, 0, RIGHT);
        for _ in 0..5 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        assert!(matches!(g.spies[1].anim.map(|a| a.state),
                         Some(round::STATE_KNOCK_FACE) | Some(round::STATE_KNOCK_BACK)),
                "a throwing spy can be knocked out: {:?}", g.spies[1].anim);
    }

    /// **stage 19a, defect 1** — `CS:279c`: a knock-out that takes over a *running* state jumps to
    /// `0x27b0`, **past** the `27a8` save. So the pose set the victim is left with when the two
    /// flinch frames end is the one the preempted action itself saved — the walk block — and not the
    /// action's own. Stage 16b saved unconditionally; a spy knocked out of a throw then came back
    /// with `0x2EF8` = `0x88` (the throw's block), so his **body** froze on a throw frame while his
    /// legs kept walking, and only a trapulator tool change (`FUN_26c0_315f` -> `code_pose`)
    /// rewrote the set. That is the owner's "frozen copy of the action sprite as the upper body".
    #[test]
    fn a_knock_out_out_of_a_throw_restores_the_walk_pose_set() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        clear_pickups(&mut g, 0);
        // the victim is mid-throw: state 2, the throw's own block 0x88
        assert!(g.start_anim(1, round::STATE_THROW, None));
        assert_eq!(g.spies[1].set, 0x88, "the throw's pose block");
        assert_eq!(g.spies[1].saved_set, 0x24,
                   "`27a8` saved the *walk* set when the throw started (he was resting)");
        throw_frame(&mut g, 0, RIGHT);
        let mut knocked = None;
        for _ in 0..14 {
            frame2(&mut g, NOKEYS, NOKEYS);
            if let Some(a) = g.spies[1].anim {
                if matches!(a.state, round::STATE_KNOCK_FACE | round::STATE_KNOCK_BACK) {
                    knocked = Some(a.state);
                    break;
                }
            }
        }
        let state = knocked.expect("the ball knocks the thrower out mid-throw");
        // while the reaction runs, the body is the flinch's own block ...
        let img = g.exe[DS_FILE_OFF..].to_vec();
        assert_eq!(g.spies[1].set, round::anim_set(&img, state), "the flinch's block");
        // ... and when it ends the body goes back to the walk block, **not** to the throw's
        for _ in 0..3 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        assert!(g.spies[1].anim.is_none(), "the two-frame reaction is over");
        assert_eq!(g.spies[1].set, 0x24,
                   "0x88 here is the stale throw block the owner reported");
        assert_eq!(g.spies[1].saved_set, 0x24, "and the preemption never overwrote the saved word");
        // the pixels follow the word: the body record is the walk block's for this frame and dir
        let ds = g.round_state().ds;
        let p = pose_of(&g, 1);
        let walk = round::PoseParams { set: 0x24, ..p };
        assert_eq!(p.records(&ds).0, walk.records(&ds).0, "the body is the walk set's record");
        assert_ne!(p.records(&ds).0,
                   round::PoseParams { set: 0x88, ..p }.records(&ds).0,
                   "and it is a different record from the throw block's frozen one");
    }

    /// The other half of `FUN_26c0_277c`: a knock-out of a spy who was **resting** does take the
    /// `27a8` save, so a carrier comes back to the pose block of the item in his hand rather than to
    /// the walk block. (`0x2EF8` = 108 with `0x2F2C` = 12 is what 7,804 recorded carrier dumps hold.)
    #[test]
    fn a_knock_out_of_a_resting_carrier_gives_him_his_carried_pose_back() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        clear_pickups(&mut g, 0);
        g.spies[1].carried = 12;
        g.spies[1].set = 108;
        g.spies[1].saved_set = 108;
        throw_frame(&mut g, 0, RIGHT);
        let mut knocked = false;
        for _ in 0..14 {
            frame2(&mut g, NOKEYS, NOKEYS);
            if matches!(g.spies[1].anim.map(|a| a.state),
                        Some(round::STATE_KNOCK_FACE) | Some(round::STATE_KNOCK_BACK)) {
                knocked = true;
                break;
            }
        }
        assert!(knocked, "a resting spy (`0x2F18` clear) is hit");
        for _ in 0..3 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        assert!(g.spies[1].anim.is_none());
        assert_eq!(g.spies[1].set, 108, "the set saved at `27a8` is the carried one");
    }

    // ---- stage 21a: point blank, and which way the ball leaves the hand ----------------------
    //
    // The owner's report: two spies **nose to nose on one row** (`|dx| = 4`, the blocking contact
    // distance of STAGE9B_REPORT.md) are hit by the spy throwing **west** and missed by the spy
    // throwing **east**, while one step apart (5) both hit. Everything below is the original's own
    // rule, and the corpus agrees with it (STAGE21A_REPORT.md).

    /// One throw at `sep` bytes on one row, thrown *towards* the other spy.
    ///
    /// The west spy stands on 100 and the east one on `100 + sep`; the thrower is whichever of the
    /// two the direction names. Returns the launch column (`0x32F3` as the throw writes it), the
    /// first column the hit test ever saw (the first one the ball was *stepped* to), and the
    /// victim's reaction if he was knocked out.
    fn point_blank(sep: i32, p1_east: bool, eastward: bool) -> (i32, Option<i32>, Option<u16>) {
        let (x1, x2) = if p1_east { (100 + sep, 100) } else { (100, 100 + sep) };
        let mut g = two_at(0, 4, x1, 44, x2);
        frame2(&mut g, NOKEYS, NOKEYS);
        let east_spy = if p1_east { 0 } else { 1 };
        let (thrower, dir) = if eastward { (1 - east_spy, RIGHT) } else { (east_spy, LEFT) };
        clear_pickups(&mut g, thrower);
        throw_frame(&mut g, thrower, dir);
        let launch = g.spies[thrower]
            .ball
            .expect("`FUN_26c0_2f87` writes the ball's three words at the throw")
            .col;
        let victim = 1 - thrower;
        let mut first = None;
        let mut reaction = None;
        for _ in 0..16 {
            frame2(&mut g, NOKEYS, NOKEYS);
            let b = g.spies[thrower]
                .ball
                .expect("`CS:30e2` clears `0x2F40`, not the column");
            if first.is_none() && b.col != launch {
                first = Some(b.col);
            }
            if let Some(a) = g.spies[victim].anim {
                if matches!(a.state, round::STATE_KNOCK_FACE | round::STATE_KNOCK_BACK) {
                    reaction = Some(a.state);
                    break;
                }
            }
        }
        (launch, first, reaction)
    }

    /// **The point-blank asymmetry.** At `|dx| = 4` a westward ball hits and an eastward one misses,
    /// with each spy on the east side in turn, so the rule is a direction and not a colour.
    ///
    /// The numbers are the executable's: `CS:3001..3009` launches a westward ball on the thrower's
    /// **own** column with `vx = -3`, `CS:2fd4..2fdf` launches an eastward one **six bytes ahead**
    /// with `vx = +3`, and `CS:3046` steps the column before `CS:307c` tests it — so the first
    /// column a westward ball is tested at is `x - 3`, inside a nose-to-nose victim's box
    /// `[x - 3, x]`, while the first column an eastward ball is tested at is `x + 9`, two bytes
    /// past that victim's box `[x + 5, x + 8]`, and each later column is further away.
    #[test]
    fn at_point_blank_only_the_westward_throw_reaches_him() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for p1_east in [false, true] {
            let (launch, first, hit) = point_blank(4, p1_east, false);
            assert_eq!(launch, 104, "`CS:3001`: a westward throw starts on the spy's own column");
            assert_eq!(first, Some(101), "`CS:3046` steps before `CS:307c` tests");
            assert_eq!(hit, Some(round::STATE_KNOCK_FACE),
                       "a westward throw at dx = -4 hits (P1 on the east: {p1_east})");

            let (launch, first, hit) = point_blank(4, p1_east, true);
            assert_eq!(launch, 106, "`CS:2fd4`: an eastward throw starts at x + 6");
            assert_eq!(first, Some(109), "the first column it is tested at is x + 9");
            assert_eq!(hit, None,
                       "an eastward throw at dx = +4 misses, and goes on missing \
                        (P1 on the east: {p1_east})");
        }
    }

    /// One step apart, both directions hit — the other half of the owner's report. At 5 the
    /// eastward ball's first tested column (`x + 9`) is the last column of the victim's box
    /// `[x + 6, x + 9]`; at 6 it is `[x + 7, x + 10]`, one inside. A 3-byte step against a 4-byte
    /// box is why every separation from 5 upwards is reachable in both directions.
    #[test]
    fn one_step_apart_both_directions_hit() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for p1_east in [false, true] {
            for sep in [5, 6] {
                assert_eq!(point_blank(sep, p1_east, false).2, Some(round::STATE_KNOCK_FACE),
                           "westward at dx = -{sep} (P1 on the east: {p1_east})");
                assert_eq!(point_blank(sep, p1_east, true).2, Some(round::STATE_KNOCK_BACK),
                           "eastward at dx = +{sep} (P1 on the east: {p1_east})");
            }
        }
    }

    /// The sharper half of the same rule: **the launch column is never tested.** At `dx = -3` the
    /// westward launch column `103` is *inside* the victim's box `[101, 104]`, and the ball still
    /// misses, because the only columns `CS:307c` ever sees are the ones `CS:3046` has already
    /// stepped to (`100`, then `97`, ...). `dx = 3` cannot be reached through the blocking
    /// collision, so this is a probe of the order alone — and the recorded point-blank knock-out
    /// says the same from the other side: `playtest5` `ds_00658` -> `ds_00660` launches at `216`
    /// with the victim's box `[213, 216]` and leaves the ball's column at **213**, not 216.
    #[test]
    fn the_launch_column_itself_is_never_tested() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let (launch, first, hit) = point_blank(3, false, false);
        assert_eq!(launch, 103);
        assert_eq!(first, Some(100), "the first test is one velocity past the box");
        assert_eq!(hit, None, "103 sits inside [101, 104] and is never tested");
    }

    /// **The recorded point-blank knock-out, replayed.** `playtest5` `ds_00659` -> `ds_00660`: the
    /// black spy (P2) at `x = 216` on the second frame of his throw, the white spy (P1) at `212` —
    /// `dx = -4`, as close as the blocking collision lets them stand — and in the next dump P1 is in
    /// `0x16` with P2's column frozen at **213** (`216 - 3`, one step).
    ///
    /// The corpus holds **no** eastward knock-out closer than `dx = +6`; the closest is
    /// `playtest5` `ds_00820` -> `ds_00821` (`dx = +6`, launch `286`, hit column `289`, `0x16`), and
    /// it is replayed the same way. That absence, over 46 throws and 52 knock-out transitions, is
    /// the measured half of the asymmetry (STAGE21A_REPORT.md §2).
    #[test]
    fn the_recorded_point_blank_knock_out_replays() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for (before, after, victim, want) in [
            ("playtest5/ds_00659.bin", "playtest5/ds_00660.bin", 0usize, round::STATE_KNOCK_FACE),
            ("playtest5/ds_00820.bin", "playtest5/ds_00821.bin", 0usize, round::STATE_KNOCK_FACE),
        ] {
            let pa = session_dir().join(before);
            let pb = session_dir().join(after);
            let b = std::fs::read(&pb).expect("the after dump");
            let st = round::RoundState::from_ds_dump(&pa).unwrap();
            let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
            g.seed_from_dump(&st, true);
            g.hatch.other = None;
            let thrower = 1 - victim;
            let mut landed = None;
            // The seeded dump is *inside* the throw, so the ball is written but `0x2F40` is still
            // clear: run until the release, the step and the test have happened, and stop when a
            // ball that had been flying stops (the hit clears `0x2F40` where it lands).
            let mut flew = false;
            for _ in 0..6 {
                frame2(&mut g, NOKEYS, NOKEYS);
                if let Some(anim) = g.spies[victim].anim {
                    if matches!(anim.state, round::STATE_KNOCK_FACE | round::STATE_KNOCK_BACK) {
                        landed = Some(anim.state);
                        break;
                    }
                }
                match g.spies[thrower].ball.map(|b| b.flying) {
                    Some(true) => flew = true,
                    _ if flew => break,
                    _ => {}
                }
            }
            assert_eq!(landed, Some(want), "{before} -> {after}");
            assert_eq!(g.spies[thrower].ball.unwrap().col,
                       w16(&b, 0x32F3 + 2 * thrower) as i16 as i32,
                       "{after}: the ball dies at the recording's own column");
            assert_eq!(g.spies[victim].x(), w16(&b, 0x329B + 2 * victim) as i16 as i32);
        }
    }

    // ---- stage 16b: the recorded snowballs ---------------------------------------------------

    fn w16(d: &[u8], o: usize) -> u16 {
        u16::from_le_bytes([d[o], d[o + 1]])
    }

    /// The words the snowball writes, read back out of the engine in the original's own places, so
    /// a replay compares like with like.
    fn snowball_words(g: &Game, i: usize) -> Vec<(&'static str, i64)> {
        let s = &g.spies[i];
        let b = s.ball.unwrap_or(Ball { row: 0, col: 0, vx: 0, flying: false });
        vec![
            ("0x32CB act", s.act_state as i64),
            ("0x32CF run", s.pose_state as i64),
            ("0x2F18 raised", s.pose_raised as i64),
            ("0x2EF8 set", s.set as i64),
            ("0x2EFC frame", s.frame as u16 as i16 as i64),
            ("0x2F0C dir", s.dir as i64),
            ("0x2F14 facing", s.mirror as i64),
            ("0x2F40 ball", b.flying as i64),
            ("0x32EF row", b.row as i64),
            ("0x32F3 col", b.col as i64),
            ("0x32FB vx", b.vx as i64),
            ("0x329B x", s.x() as i64),
            ("0x3297 y", s.y() as i64),
        ]
    }

    fn recorded_words(d: &[u8], i: usize) -> Vec<(&'static str, i64)> {
        let o = 2 * i;
        vec![
            ("0x32CB act", w16(d, 0x32CB + o) as i64),
            ("0x32CF run", w16(d, 0x32CF + o) as i64),
            ("0x2F18 raised", w16(d, 0x2F18 + o) as i64),
            ("0x2EF8 set", w16(d, 0x2EF8 + o) as i64),
            ("0x2EFC frame", w16(d, 0x2EFC + o) as i16 as i64),
            ("0x2F0C dir", w16(d, 0x2F0C + o) as i64),
            ("0x2F14 facing", w16(d, 0x2F14 + o) as i64),
            ("0x2F40 ball", (w16(d, 0x2F40 + o) != 0) as i64),
            ("0x32EF row", w16(d, 0x32EF + o) as i64),
            ("0x32F3 col", w16(d, 0x32F3 + o) as i64),
            ("0x32FB vx", w16(d, 0x32FB + o) as i16 as i64),
            ("0x329B x", w16(d, 0x329B + o) as i64),
            ("0x3297 y", w16(d, 0x3297 + o) as i64),
        ]
    }

    /// **Every recorded throw.** Each dump pair whose *after* dump holds a throw on its first or
    /// second drawn frame is replayed from the before dump with the gesture those words imply (the
    /// direction is the facing the throw itself set) and every word the routine writes is compared.
    ///
    /// The port needs the button-A frame before the button-B one — one key standing in for two — so
    /// the recording's frame 0 is the port's *second* frame; the dump's own `0x2EFC`/2 extra frames
    /// are then run so the phase matches too.
    #[test]
    fn every_recorded_throw_replays_byte_for_byte() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["audio-smoke-2", "m1-fix-960", "m3-heat", "playtest5"];
        let mut tried = 0usize;
        let mut ok = 0usize;
        let mut skipped = 0usize;
        let mut tapped = 0usize;
        for ses in sessions {
            let ds = dumps_of(ses);
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for i in 0..2usize {
                    let o = 2 * i;
                    // the after dump is the throw's own first or second drawn frame
                    if w16(&b, 0x2F18 + o) == 0
                        || w16(&b, 0x32CF + o) != round::STATE_THROW
                        || w16(&b, 0x2F40 + o) != 0
                        || !matches!(w16(&b, 0x2EFC + o) as i16, 0 | 2)
                        || w16(&b, 0x2F64 + o) >= 2
                        || w16(&b, 0x2F70 + o) != 0
                    {
                        continue;
                    }
                    // the before dump must be idle, empty-handed, on open ground, with nothing to
                    // pick up under the feet and the trapulator shut
                    if w16(&a, 0x2F18 + o) != 0
                        || w16(&a, 0x32CF + o) == round::STATE_THROW
                        || w16(&a, 0x2F2C + o) != 0xFFFF
                        || w16(&a, 0x2F38 + o) != 0
                        || w16(&a, 0x2F64 + o) != 0
                        || w16(&a, 0x2F40 + o) != 0
                        // a throw freezes him, so the pair must not straddle a step
                        || w16(&a, 0x329B + o) != w16(&b, 0x329B + o)
                        || w16(&a, 0x3297 + o) != w16(&b, 0x3297 + o)
                        || w16(&a, 0x329F + o) != w16(&b, 0x329F + o)
                    {
                        skipped += 1;
                        continue;
                    }
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    for k in 0..2 {
                        g.spies[k].anim = None;
                        g.spies[k].pose_raised = false;
                        g.spies[k].ball = None;
                    }
                    g.hatch.other = None;
                    let dir = if w16(&b, 0x2F14 + o) != 0 { LEFT } else { RIGHT };
                    let extra = w16(&b, 0x2EFC + o) as usize / 2;
                    // The port's one fire key runs the button-A **tap** before the button-B
                    // gesture, and a tap standing on an item takes it (the original has two
                    // buttons and never had to). Those pairs cannot be replayed by this mapping:
                    // they are counted as skipped, not hidden (STAGE16B_REPORT.md §4.3).
                    frame_p(&mut g, i, Keys { fire: true, ..NOKEYS });
                    if g.spies[i].carried != CARRY_NONE {
                        tapped += 1;
                        continue;
                    }
                    frame_p(&mut g, i, Keys { fire: true, ..dir });
                    if g.spies[i].anim.map(|a| a.state) != Some(round::STATE_THROW) {
                        panic!("{}/{} P{}: the throw did not start (ball {:?})", ses,
                               w[1].file_name().unwrap().to_string_lossy(), i + 1, g.spies[i].ball);
                    }
                    tried += 1;
                    for _ in 0..extra {
                        frame2(&mut g, NOKEYS, NOKEYS);
                    }
                    let got = snowball_words(&g, i);
                    let want = recorded_words(&b, i);
                    let mut bad = Vec::new();
                    for ((n, g), (_, w)) in got.iter().zip(want.iter()) {
                        if g != w {
                            bad.push(format!("{n}: engine {g}, recording {w}"));
                        }
                    }
                    if bad.is_empty() {
                        ok += 1;
                    } else {
                        panic!("{}/{} P{}: {}", ses,
                               w[1].file_name().unwrap().to_string_lossy(), i + 1, bad.join("; "));
                    }
                }
            }
        }
        eprintln!("stage 16b: {ok} of {tried} recorded throws replayed ({skipped} pairs not a clean \
throw start, {tapped} whose ground held an item the one-key tap would take)");
        assert!(tried >= 25, "expected the corpus's throws, found {tried}");
        assert_eq!(ok, tried);
    }

    /// **Every recorded snowball knock-out.** Each dump pair whose after dump shows a spy entering
    /// `0x16`/`0x18` while the *other* spy's ball was already in flight is replayed with one frame
    /// and no keys. The knock-out lands in the draw pass, after both frame bodies, so the victim's
    /// animation is still on its seed frame `-2` in the recording — and must be in the engine too.
    #[test]
    fn every_recorded_snowball_knock_out_replays_byte_for_byte() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["audio-smoke-2", "m1-fix-960", "playtest5"];
        let mut tried = 0usize;
        let mut ok = 0usize;
        let mut kinds = [0usize; 2];
        let mut seeded = 0usize;
        for ses in sessions {
            let ds = dumps_of(ses);
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for victim in 0..2usize {
                    let o = 2 * victim;
                    let vo = 2 * (1 - victim);
                    let run = w16(&b, 0x32CF + o);
                    if !matches!(run, 0x16 | 0x18) || w16(&a, 0x32CF + o) == run {
                        continue;
                    }
                    // the ball that does it must be in flight in the before dump, with a velocity
                    let vx = w16(&a, 0x32FB + vo) as i16;
                    if w16(&a, 0x2F40 + vo) == 0 || vx.abs() != BALL_SPEED as i16 {
                        continue;
                    }
                    // neither spy moves on the frame the ball lands (the victim is frozen by the
                    // animation), so a pair that straddles a step is not a single-frame replay
                    if w16(&a, 0x329B + o) != w16(&b, 0x329B + o)
                        || w16(&a, 0x3297 + o) != w16(&b, 0x3297 + o)
                        || w16(&a, 0x329B + vo) != w16(&b, 0x329B + vo)
                        || w16(&a, 0x3297 + vo) != w16(&b, 0x3297 + vo)
                    {
                        continue;
                    }
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    g.hatch.other = None;
                    // The ring samples at ~10 Hz against 9.23 frames a second, so the dump that
                    // holds the knock-out can be one or two frames after the ball left the before
                    // dump. Run until the reaction appears (at most six frames), then compare the
                    // words *that* frame holds — the recording is the oracle for the state, not for
                    // the ring's own timing.
                    tried += 1;
                    let mut landed = false;
                    for _ in 0..6 {
                        frame2(&mut g, NOKEYS, NOKEYS);
                        if matches!(g.spies[victim].anim.map(|a| a.state),
                                    Some(round::STATE_KNOCK_FACE) | Some(round::STATE_KNOCK_BACK))
                        {
                            landed = true;
                            break;
                        }
                        if g.spies[1 - victim].ball.map(|b| b.flying) != Some(true) {
                            break;
                        }
                    }
                    if !landed {
                        panic!("{}/{} victim P{}: the ball never landed ({:?})", ses,
                               w[1].file_name().unwrap().to_string_lossy(), victim + 1,
                               g.spies[1 - victim].ball);
                    }
                    let got = snowball_words(&g, victim);
                    let want = recorded_words(&b, victim);
                    let mut bad = Vec::new();
                    for ((n, gv), (_, wv)) in got.iter().zip(want.iter()) {
                        // the ball's three words belong to the victim's *own* throw (he may be
                        // throwing when the other spy's ball arrives) — the hit writes none of them
                        if matches!(*n, "0x2F40 ball" | "0x32EF row" | "0x32F3 col" | "0x32FB vx") {
                            continue;
                        }
                        if *n == "0x2EFC frame" {
                            // The pass is per player (stage 24d): a ball of player 2's lands
                            // before player 1's body, so the victim is on the *second* step of
                            // the reaction (0) by the end of the frame; a ball of player 1's
                            // lands after player 2's body and leaves the seed frame (-2). The
                            // dump ring samples at ~10 Hz against 9.23 frames a second, so a pair
                            // can also catch either of the two drawn frames (2).
                            if !matches!(*gv, -2 | 0) || !matches!(wv, -2 | 0 | 2) {
                                bad.push(format!("{n}: engine {gv}, recording {wv}"));
                            } else if *wv == -2 {
                                seeded += 1;
                            }
                            continue;
                        }
                        if gv != wv {
                            bad.push(format!("{n}: engine {gv}, recording {wv}"));
                        }
                    }
                    // the ball is consumed by the hit
                    if g.spies[1 - victim].ball.map(|b| b.flying) == Some(true) {
                        bad.push("the ball is still flying".into());
                    }
                    kinds[if run == 0x16 { 0 } else { 1 }] += 1;
                    if bad.is_empty() {
                        ok += 1;
                    } else {
                        panic!("{}/{} victim P{}: {}", ses,
                               w[1].file_name().unwrap().to_string_lossy(), victim + 1,
                               bad.join("; "));
                    }
                }
            }
        }
        eprintln!("stage 16b: {ok} of {tried} recorded snowball knock-outs replayed (0x16 {}, 0x18 {}, {seeded} caught on the seed frame)", kinds[0], kinds[1]);
        assert!(tried >= 20, "expected the corpus's knock-outs, found {tried}");
        assert_eq!(ok, tried);
        assert!(kinds[0] >= 10 && kinds[1] >= 1, "both faces of the hit: {kinds:?}");
        assert!(seeded >= 3, "the seed frame -2 is what the recordings hold: {seeded}");
    }

    /// A stick *I* set carries my colour's tag (`0x28` white / `0x48` black) and comes back as
    /// plain dynamite: `CS:37aa` masks the byte to `0x1F`, sees 8 and clears the cell.
    #[test]
    fn my_own_dynamite_trap_comes_back_as_dynamite() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let img = exe();
        let tag = items::player_tag(&img[DS_FILE_OFF..], 0);
        for byte in [8 | tag, 8 | tag | items::BURIED] {
            let mut g = plain();
            let a = put_underfoot(&mut g, byte);
            fire_frames(&mut g, FIRE, 1);
            assert_eq!(g.spies[0].carried, 8, "my tag on {byte:#04x}");
            assert_eq!(g.ds[a], 0, "the cell is cleared");
        }
    }

    /// The object cell arithmetic, against the recorded `DS:0x32D7` (the pointer the original
    /// keeps): screen 4, x 71, y 42 -> `0x589C + 0x9C + 35 = 0x595B`.
    #[test]
    fn the_object_cell_matches_the_recorded_pointer() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let ds = exe();
        let img = &ds[DS_FILE_OFF..];
        assert_eq!(items::cell_addr(img, 4, 71, 42), Some(0x595B));
        let p = crate::orig::require("recon/round/sessions/autoplay-pickup/ds_00004.bin");
        if let Ok(d) = std::fs::read(&p) {
            let rec = u16::from_le_bytes([d[0x32D7], d[0x32D8]]) as usize;
            let scr = u16::from_le_bytes([d[0x329F], d[0x32A0]]);
            let x = u16::from_le_bytes([d[0x329B], d[0x329C]]) as i32;
            let y = u16::from_le_bytes([d[0x3297], d[0x3298]]) as i32;
            assert_eq!(items::cell_addr(&d, scr, x, y), Some(rec),
                       "the port's cell must be the original's own pointer");
        }
    }

    #[test]
    fn state_json_reports_the_step_count_as_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        hold(&mut g, RIGHT, 7);
        let j = g.state_json();
        assert!(j.contains("\"frame\":7"), "{j}");
        assert!(j.contains("\"scene\":\"round-play\""));
        assert!(j.contains("\"island\":1"));
    }

    /// **The shared-screen rule**: the visitor is the spy who walks into the *other spy's window*
    /// (`map_x - 4 < x < map_x + 0x29`), and he stops being one the moment he walks out of it.
    /// Two spies on the same screen but far apart both see their own view (`DS:0x2F7E` clear on
    /// both) — which is what the owner expects and what 831 of the 1,464 same-screen recorded rows
    /// show.
    #[test]
    fn the_visitor_flag_follows_the_45_unit_window() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(80),
                                     ..RoundOpts::default() });
        // put them on one screen, far apart: neither console is blanked
        g.spies[0].screen = 4;
        g.spies[1].screen = 4;
        g.spies[0].x3 = 40 * SUB;
        g.spies[1].x3 = 200 * SUB;
        g.spies[0].map_x = 20;
        g.spies[1].map_x = 180;
        g.spies[0].blank = false;
        g.spies[1].blank = false;
        g.update_visitor(0);
        assert!(!g.spies[0].blank, "40 is outside 180-4 .. 180+0x29: no blanking");
        assert!(!g.spies[1].blank);
        assert!(g.strip_for(0, false).iter().any(|&b| b != 0x55), "player 1 sees his room");

        // walk player 0 into player 1's window: he becomes the visitor
        g.spies[0].x3 = 190 * SUB;
        g.update_visitor(0);
        assert!(g.spies[0].blank, "190 is inside 176 .. 221");
        assert!(!g.spies[1].blank, "the host keeps his view");
        assert!(g.strip_for(0, false).iter().all(|&b| b == 0x55), "the visitor's console is flat");

        // and walking back out of the window clears it again, with his own scroll 33 units away
        g.spies[0].x3 = 230 * SUB;
        g.update_visitor(0);
        assert!(!g.spies[0].blank, "230 is past the window: the blanking clears");
        assert_eq!(g.spies[0].map_x, 180 + VISITOR_KEEP_OFFSET);
        assert!(g.strip_for(0, false).iter().any(|&b| b != 0x55), "and he sees his own view");

        // the window's two edges, exactly as the code compares them
        g.spies[1].map_x = 100;
        g.spies[0].x3 = 97 * SUB; // 100 - 4 + 1
        g.spies[0].blank = false;
        g.update_visitor(0);
        assert!(g.spies[0].blank, "x = map_x - 3 is inside the window");
        g.spies[0].x3 = 96 * SUB; // 100 - 4
        g.update_visitor(0);
        assert!(!g.spies[0].blank, "x = map_x - 4 is outside it");
        g.spies[0].x3 = 140 * SUB; // 100 + 0x28
        g.update_visitor(0);
        g.spies[0].x3 = 141 * SUB; // 100 + 0x29
        g.update_visitor(0);
        assert!(!g.spies[0].blank, "x = map_x + 0x29 is outside it");
    }

    /// **Stage 24e — a raised spy still runs the frame's tail, visitor test included.**
    ///
    /// `CS:0727` sends a spy whose `0x2F18` is set to `0x97c`, which runs `35e5`, `2392` and the
    /// script's own step and then falls into the **same** `0xa1a` commit / `0xa41`
    /// `FUN_26c0_0000` the walking path uses. The visitor re-test lives in that routine's tail
    /// (`0x174..0x202`), so a spy who is sinking, knocked out or laughing still takes — and still
    /// keeps — the visitor flag. The demo is the measurement: spy 2 goes under at frame 466 and the
    /// recording hands *him* `0x2F80` at 479, while the port used to skip his tail and hand the flag
    /// to spy 1 instead, freezing spy 1's scroll for the next two hundred frames.
    #[test]
    fn a_raised_spy_still_takes_the_visitor_flag() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(108),
                                     start_x2: Some(114), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10]; // open ground: the rule under test is the flag, not terrain
        g.spies[1].y3 = 46 * SUB;
        // spy 1's view 76 puts spy 2's x 114 inside (72, 117); spy 2's view 108 puts spy 1's x 108
        // inside (104, 149) too, so the *order* decides — and player 2's frame runs first.
        g.spies[0].map_x = 76;
        g.spies[1].map_x = 108;
        g.spies[0].blank = false;
        g.spies[1].blank = false;
        g.start_anim(1, round::STATE_LAUGH, None);
        assert!(g.spies[1].pose_raised, "spy 2 is in an animation");

        for _ in 0..FRAME_TICKS {
            g.tick(Input::default());
        }
        assert!(g.spies[1].blank, "the raised spy is the visitor: the recording's 0x2F80 = 1");
        assert!(!g.spies[0].blank, "and the walking spy is not");
    }

    /// **Stage 24g — the walk in step will not drag a partner who is holding the second button.**
    ///
    /// `CS:0a4c..0x0afd`, the tail of the move loop, in its own words:
    ///
    /// ```text
    /// a4c  if (this.0x31A6 == 0) done            ; this spy's move did not scroll the view
    /// a5c  if (0x2F7E + 0x2F80 == 0) done        ; nobody on the other's board
    /// a65  di = 0x3283(other); if (*(di) == 1) done   ; <-- the SECOND button
    /// a6e  if (other.0x2F18 != 0) done           ; raised
    /// a75  if (other.0x2F64 != 0) done           ; out of the round
    /// a7c  if (right(this) == 1 && right(other) == 1) other.x += 1
    /// ```
    ///
    /// The word at `0xa65` is `0x3283`, which is **B**; `0x327F` (A) is not tested anywhere in the
    /// routine. A spy on B skips his own move block (`CS:0791`: `FUN_26c0_2d7e`, then `jmp
    /// 0x836`), so he stands still with a direction down — and the shove has to leave him there.
    /// The port tested `keys.fire` (A), so a held-B partner rode the host's scroll one unit a
    /// frame. The demo is the measurement: spy 2 holds `right+B` from frame 709 to 732 while spy 1
    /// walks east out of room 6, and the recording holds spy 2 at x **76** for all of it while the
    /// port rode him to 90 — which put him back inside spy 1's visitor window, so his `0x2F80`
    /// and his view stayed at 108 until the button came up (the recording drops both at 721).
    #[test]
    fn the_walk_in_step_refuses_a_partner_on_the_second_button() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(95), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10]; // open ground: the rule under test is the shove
        g.spies[1].y3 = 46 * SUB;    // one row down: the two bodies do not block each other
        // spy 1's view well to his left, so his eastward step scrolls every frame (the shove's
        // own gate at `0xa4c`); spy 2 at 95 is inside its window (56, 101)
        g.spies[0].map_x = 60;
        g.spies[1].map_x = 60;
        let right = Keys { right: true, ..NOKEYS };
        let right_a = Keys { right: true, fire: true, ..NOKEYS };
        let right_b = Keys { right: true, fire_b: true, ..NOKEYS };

        // one frame of nothing: spy 2 is inside spy 1's window and becomes the visitor (`0x2F80`),
        // which is the other gate the shove tests (`0xa5c`)
        frame2(&mut g, NOKEYS, NOKEYS);
        assert!(g.spies[1].blank, "player 2 is the visitor");
        assert!(!g.spies[0].blank, "the host keeps his console");

        // **B held**: the shove is refused, so the partner stays where he is. This is the demo's
        // own state — spy 2 holds `right+B` for frames 709..732 while spy 1 walks east.
        let start = g.spies[1].x();
        g.push_in_step(0, [right, right_b]);
        assert_eq!(g.spies[1].x(), start, "a partner on B (0x3283) is not shoved");

        // **A held**: `0x327F` is not the word the routine tests, so the shove runs.
        g.push_in_step(0, [right, right_a]);
        assert_eq!(g.spies[1].x(), start + 1, "a partner on A (0x327F) is shoved");

        // ...and with the second button up (both spies simply walking) the shove runs too.
        g.push_in_step(0, [right, right]);
        assert_eq!(g.spies[1].x(), start + 2, "a walking partner is shoved");

        // the other two gates, for the record: the shove also needs a visitor (`0xa5c`) and a
        // partner who is not raised (`0xa6e`)
        g.spies[1].blank = false;
        g.spies[0].blank = false;
        let x = g.spies[1].x();
        g.push_in_step(0, [right, right]);
        assert_eq!(g.spies[1].x(), x, "nobody on the other's board: no shove");
        g.spies[1].blank = true;
        g.spies[1].pose_raised = true;
        g.push_in_step(0, [right, right]);
        assert_eq!(g.spies[1].x(), x, "a raised partner is not shoved");
    }

    /// **Stage 24g — a spy on the second button still runs the frame's tail.**
    ///
    /// `CS:079d` (the `FUN_26c0_2d7e` call) is a `jmp 0x836`, and `0x836` is the *pre-move* entry
    /// of the frame body: the hazard pass, the positional commit (`0xa1a`), the terrain probe
    /// (`0xa3e`) and `FUN_26c0_0000` (`0xa41`, whose tail `0x174..0x202` is the visitor re-test)
    /// all run for a spy who is holding B with a direction. The port returned straight out of the
    /// B branch, so such a spy never re-tested his flag: the demo's spy 2 holds `right+B` for
    /// frames 709..732 while spy 1's view walks east past him, and the recording drops `0x2F80`
    /// at 721 (`x 76 <= map(host) 80 - 4`, `CS:1c3`) with his view set to `map - 0x21` = 47 while
    /// the port kept both at 108.
    #[test]
    fn a_spy_on_the_second_button_still_runs_the_visitor_tail() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // The demo's own geometry, one room over: the host at x 100 with his view at 60 — past
        // the scroll dead zone (`x > map + 30`, `CS:005e`), so every step he takes slides the
        // window 2 units — and the visitor inside it at 95 (`map-4`, `map+0x29` = 56, 101).
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(95), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        g.spies[1].y3 = 46 * SUB;   // not on the host's row: `35e5` never blocks the host
        g.spies[0].map_x = 60;
        g.spies[1].map_x = 60;
        frame2(&mut g, NOKEYS, NOKEYS);
        assert!(g.spies[1].blank, "player 2 starts inside the window, so he is the visitor");

        // spy 1 walks east; player 2 holds right+B and therefore stands still every frame.
        let x2 = g.spies[1].x();
        let right = Keys { right: true, ..NOKEYS };
        let right_b = Keys { right: true, fire_b: true, ..NOKEYS };
        let mut cleared = None;
        for f in 0..20 {
            // the host's view at the top of the frame is the one player 2's body sees (player 2
            // runs first, `CS:0702`), and the host scrolls later in the same frame
            let host_before = g.spies[0].map_x;
            frame2(&mut g, right, right_b);
            assert_eq!(g.spies[1].x(), x2, "frame {f}: the B spy did not move");
            if cleared.is_none() && !g.spies[1].blank {
                cleared = Some((f, host_before, g.spies[1].map_x));
            }
        }
        let (f, host, own) = cleared.expect(
            "the host's scroll carries spy 2 out of the window and his flag clears \
             (`CS:1c3` -> `CS:1dd`) even though he never left the B branch");
        assert!(x2 <= host - VISITOR_ENTRY_LEFT,
                "frame {f}: out on the left (x {x2}, host view {host})");
        assert_eq!(own, host - VISITOR_KEEP_OFFSET,
                   "and his own view is the host's minus 0x21");
    }

    /// **The owner's defect C.** The host walks away from a visitor who is standing still: the
    /// visitor's console must come back. `FUN_26c0_0000` — which holds the window test — is on the
    /// **common** path of the original's per-player loop, so it runs for a spy who is holding
    /// nothing: the flag is re-tested every frame for both spies and the host's own scroll is what
    /// pushes the visitor out of the window.
    #[test]
    fn the_host_walking_away_clears_the_visitor_console() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(120), start_y: Some(44),
                                     ..RoundOpts::default() });
        // an empty room: the room-4 island terrain has the igloo in the way of a long walk
        g.levels = [[0u16; 12]; 10];
        // **one row down.** The window rule this test pins is purely horizontal, and since stage 9b
        // the two bodies block each other on one row (`CS:35e5`: `|Δy| <= 1 && |Δx| <= 3`), so a
        // host walking *through* the visitor needs the other row. That the pass is allowed is the
        // original's own rule, not a concession: the corpus has the two spies at dx 0 on rows 2
        // apart (`stage9b_probe.py`).
        g.spies[1].y3 = 46 * SUB;
        assert_eq!((g.spies[0].x(), g.spies[1].x()), (100, 120));
        // one frame with nothing held: player 2 is inside player 1's window and becomes the visitor
        hold(&mut g, Keys::default(), FRAME_TICKS as usize);
        assert!(g.spies[1].blank, "player 2 is the visitor");
        assert!(!g.spies[0].blank, "the host keeps his console");
        let idle_x = g.spies[1].x();
        let idle_map = g.spies[1].map_x;

        // player 1 walks east (away). Player 2 holds nothing and does not move at all.
        let frames = until(&mut g, RIGHT, 40 * FRAME_TICKS as usize,
                           |g| !g.spies[1].blank);
        assert!(frames < 40 * FRAME_TICKS as usize,
                "the console must come back without the visitor moving");
        assert_eq!(g.spies[1].x(), idle_x, "the visitor did not move");
        assert_ne!(g.spies[1].map_x, idle_map, "his view is set for the wake-up");
        assert!(g.spies[0].map_x > idle_x + VISITOR_ENTRY_LEFT,
                "the host's scroll is what took him out of the window");
        // defect D: the view he wakes up with contains him, inside the scroll dead zone
        let rel = g.spies[1].x() - g.spies[1].map_x;
        assert!((0..STRIP_PITCH as i32).contains(&rel), "the spy is on his own view: rel {rel}");
        // he left through the *left* edge (the host walked east), so his own scroll is
        // `host.map_x - 0x21` and his x is `host.map_x - 4`: rel = 0x21 - 4 = 29, inside 6..30
        assert_eq!(rel, VISITOR_KEEP_OFFSET + VISITOR_ENTRY_LEFT);
        assert!(!g.strip_for(1, false).iter().all(|&b| b == 0x55), "and the console is not blank");
    }

    /// **The recorded igloo entries**, all of them: every `screen -> screen + 10` transition in the
    /// corpus must satisfy the walk-in model exactly — the spy stood in the alcove (`cx` 21..22,
    /// `y` 40..44), and the interior position is `colbase + 14`, `y 48`, view `colbase - 8`. That is
    /// `FUN_26c0_1e0f`'s `x = colbase + 0xF` plus the commit's one unit west, and it is why stage 4's
    /// reading (an action key at the alcove) looked plausible: the alcove *is* the cell in front of
    /// the door.
    #[test]
    fn every_recorded_igloo_entry_matches_the_walk_in_model() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sess = crate::orig::require("recon/round/sessions");
        if !sess.exists() {
            eprintln!("skip: no recorded sessions");
            return;
        }
        let mut entries = 0;
        let mut bad = Vec::new();
        for dir in std::fs::read_dir(&sess).expect("sessions") {
            let dir = dir.expect("entry").path();
            let Ok(text) = std::fs::read_to_string(dir.join("state.csv")) else { continue };
            let all = dump_column(&text);
            let names: Vec<&str> = all.iter().map(|s| s.as_str())
                .filter(|n| n.starts_with("ds_") && n.ends_with(".bin"))
                .collect();
            // The dump ring is not always one frame: a pair can be 130 ms apart and a spy walks two
            // units a frame, so the *previous* dump can sit that far short of the door. Read the
            // stride off the state stream rather than assuming one frame — `orig-compare-1`'s ring
            // runs at 7.5 Hz and produced a `cx 24` entry (the landing was exact; the approach was
            // not sampled).
            let mono_of = |name: &str| -> Option<f64> {
                let mut lines = text.lines();
                let head: Vec<&str> = lines.next()?.split(',').collect();
                let it = head.iter().position(|c| *c == "mono")?;
                let id = head.iter().position(|c| *c == "dump")?;
                lines.find_map(|l| {
                    let f: Vec<&str> = l.split(',').collect();
                    if f.get(id)?.trim() != name {
                        return None;
                    }
                    f.get(it)?.trim().parse::<f64>().ok()
                })
            };
            for pair in names.windows(2) {
                let Some(pre) = session_dump(dir.file_name().unwrap().to_str().unwrap(), pair[0])
                    else { continue };
                let Some(post) = session_dump(dir.file_name().unwrap().to_str().unwrap(), pair[1])
                    else { continue };
                let frames = match (mono_of(pair[0]), mono_of(pair[1])) {
                    // a frame is 108.342 ms, not 100 (stage 16a): the dump spacing is turned into
                    // frames with the engine's own period, not with the old 10 Hz assumption.
                    (Some(a), Some(b)) => ((b - a) / (FRAME_MS / 1000.0)).round().max(1.0) as i32,
                    _ => 1,
                };
                let slack = 2 * frames; // two units a frame, and he approaches from the east
                for i in 0..2usize {
                    let a = pre.players[i];
                    let b = post.players[i];
                    if a.band % 2 != 0 || a.band >= FIRST_INTERIOR_SCREEN
                        || b.band != a.band + 10
                    {
                        continue;
                    }
                    entries += 1;
                    let x = a.spy.x as i32;
                    let base = 26 * (x / 26).clamp(0, 11);
                    let cx = x - base;
                    let want = (b.band, base + 14, 48, base - 8);
                    let got = (b.band, b.spy.x as i32, b.spy.y as i32, b.map_x as i32);
                    if want != got
                        || !(21..=22 + slack).contains(&cx)
                        || !(40..=44).contains(&(a.spy.y as i32))
                    {
                        bad.push(format!(
                            "{}/{} p{}: pre cx {cx} y {} ({frames} frames) -> want {want:?} got {got:?}",
                            dir.file_name().unwrap().to_string_lossy(), pair[1],
                            i + 1, a.spy.y));
                    }
                }
            }
        }
        assert!(entries >= 30, "only {entries} recorded entries found");
        assert!(bad.is_empty(), "{} of {entries} entries disagree: {:?}", bad.len(),
                &bad[..bad.len().min(5)]);
        eprintln!("{entries} recorded igloo entries, all on the walk-in model");
    }

    /// **The recorded igloo exits**, the other half of the same `FUN_26c0_1e0f` switch: walking
    /// east onto the interior mask's code 8 cell (`cx` 17, `y` 46..48) fires `screen - 10`,
    /// `x = colbase + 0x16`, `y = 0x2A`, and then the caller's commit and its scroll-retry carry the
    /// spy to `colbase + 24` (view `colbase - 6`) — the whole sequence, replayed through the engine
    /// from the dump one frame earlier.
    #[test]
    fn every_recorded_igloo_exit_replays_through_the_engine() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sess = crate::orig::require("recon/round/sessions");
        if !sess.exists() {
            eprintln!("skip: no recorded sessions");
            return;
        }
        let mut exits = 0;
        let mut matched = 0;
        let mut bad = Vec::new();
        for dir in std::fs::read_dir(&sess).expect("sessions") {
            let dir = dir.expect("entry").path();
            let Ok(text) = std::fs::read_to_string(dir.join("state.csv")) else { continue };
            let all = dump_column(&text);
            let names: Vec<&str> = all.iter().map(|s| s.as_str())
                .filter(|n| n.starts_with("ds_") && n.ends_with(".bin"))
                .collect();
            let ses = dir.file_name().unwrap().to_string_lossy().to_string();
            for pair in names.windows(2) {
                let (Some(pre), Some(post)) = (session_dump(&ses, pair[0]), session_dump(&ses, pair[1]))
                    else { continue };
                for i in 0..2usize {
                    let (a, b) = (pre.players[i], post.players[i]);
                    if a.band < FIRST_INTERIOR_SCREEN || b.band + 10 != a.band {
                        continue;
                    }
                    exits += 1;
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&pre, true);
                    // the key that was held: east, onto the cell
                    for _ in 0..FRAME_TICKS {
                        g.tick(Input {
                            p1: if i == 0 { RIGHT } else { Keys::default() },
                            p2: if i == 0 { Keys::default() } else { RIGHT },
                        });
                    }
                    let sp = &g.spies[i];
                    let want = (b.band, b.spy.x, b.spy.y);
                    let got = (sp.screen, sp.x() as u16, sp.y() as u16);
                    if want == got {
                        matched += 1;
                    } else {
                        bad.push(format!("{ses}/{} p{}: want {want:?} got {got:?}",
                                         pair[1], i + 1));
                    }
                }
            }
        }
        assert!(exits >= 25, "only {exits} recorded exits found");
        eprintln!("{exits} recorded igloo exits, {matched} replay exactly");
        assert!(matched * 4 >= exits * 3, "{} of {exits} exits replay: {:?}", matched,
                &bad[..bad.len().min(5)]);
    }

    /// `--visitor-eval-mover-only` restores stage 4's behaviour, where the test only ran in the
    /// moving spy's frame: the same walk leaves the visitor's console blank until *he* moves.
    #[test]
    fn the_mover_only_flag_keeps_the_stale_blank() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(120), start_y: Some(44),
                                     visitor_eval_mover_only: true,
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        // the visitor one row down, so the host can walk past him (stage 9b: same row = blocked)
        g.spies[1].y3 = 46 * SUB;
        // under the old rule a spy must *move* for his frame to test the window at all
        for _ in 0..FRAME_TICKS {
            g.tick(Input { p1: Keys::default(), p2: LEFT });
        }
        assert!(g.spies[1].blank);
        hold(&mut g, RIGHT, 40 * FRAME_TICKS as usize);
        assert!(g.spies[1].blank, "the stage-4 rule holds the blank while the visitor stands still");
        // and the visitor's own movement clears it, which is what stage 4 did
        until(&mut g, Keys::default(), 4, |_| false);
        for _ in 0..40 * FRAME_TICKS as usize {
            g.tick(Input { p1: Keys::default(), p2: LEFT });
            if !g.spies[1].blank {
                break;
            }
        }
        assert!(!g.spies[1].blank, "moving clears it even under the old rule");
    }

    // ---- stage 5: the fourth defect (the drawing of the visitor) --------------------------

    /// **The owner's defect B.** When spy A is inside spy B's window, A's console blanks (measured:
    /// flat `0x55`) and A must be **drawn into B's console**. The recorded dumps pin the rule: for
    /// the 849 dumps where player 2 is the visitor, `DS:0x1A92` holds player 1's console (the draw
    /// loop composes console 2 and then console 1), so the engine's player-1 strip must be byte
    /// exact — with the visitor's sprite at `visitor.x - host.map_x`, in the visitor's own pose and
    /// his own sprite bank.
    #[test]
    fn the_visitor_is_drawn_on_the_host_console() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for (ses, name) in [
            ("audio-smoke", "ds_00065.bin"),   // visitor 40 units in: at the right edge
            ("item-check-2", "ds_00421.bin"),  // 7 units in
            ("playtest2", "ds_00761.bin"),     // 3 units in
            ("playtest4", "ds_01618.bin"),     // 23 units in
            ("playtest5", "ds_00694.bin"),     // -3: half the sprite off the left edge
        ] {
            let Some(st) = session_dump(ses, name) else { continue };
            assert!(st.players[1].blank && !st.players[0].blank && st.players[1].band < 10,
                    "{ses}/{name}: not a visitor dump");
            // two spies: `seed_from_dump` seeds player 2 only in a two-player round, and the
            // visitor draw needs both (the recorded corpus was always driven with two)
            let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
            g.seed_from_dump(&st, true);
            let want = &st.ds[round::DS_STRIP..round::DS_STRIP + STRIP_LEN];
            let with = g.engine_strip(0);
            let diff = with.iter().zip(want).filter(|(a, b)| a != b).count();
            assert_eq!(diff, 0, "{ses}/{name}: the host's console must hold both spies");
            // and stage 4's composition (own spy only) is what it fixes: the visitor's absence is
            // a visible patch, not a rounding difference
            g.visitor_draw = false;
            let without = g.engine_strip(0);
            let diff = without.iter().zip(want).filter(|(a, b)| a != b).count();
            assert!(diff > 0, "{ses}/{name}: without the visitor draw there must be a residual");
        }
    }

    /// The same rule, over the corpus rather than five dumps: for every thirteenth dump in the
    /// sessions where player 2 is the visitor, the host's console must compose the recorded strip.
    /// Stage 4 closed 2 of 61 of these; the visitor draw closes 23 — the rest are the known
    /// raised/climb pose gap, not the visitor (`STAGE5_REPORT.md` has the breakdown).
    #[test]
    fn the_visitor_draw_closes_the_recorded_host_consoles() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sess = crate::orig::require("recon/round/sessions");
        if !sess.exists() {
            eprintln!("skip: no recorded sessions");
            return;
        }
        let mut checked = 0;
        let mut with = 0;
        let mut without = 0;
        for dir in std::fs::read_dir(&sess).expect("sessions") {
            let dir = dir.expect("entry").path();
            let Ok(text) = std::fs::read_to_string(dir.join("state.csv")) else { continue };
            let names = dump_column(&text);
            let mut n = 0;
            for name in names {
                let name = name.as_str();
                if !name.starts_with("ds_") || !name.ends_with(".bin") {
                    continue;
                }
                n += 1;
                if n % 13 != 0 {
                    continue;
                }
                let Some(st) = session_dump(dir.file_name().unwrap().to_str().unwrap(), name) else {
                    continue;
                };
                let host = &st.players[0];
                let vis = &st.players[1];
                if !(vis.blank && !host.blank && vis.band == host.band && host.band < 10) {
                    continue;
                }
                let want = &st.ds[round::DS_STRIP..round::DS_STRIP + STRIP_LEN];
                let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                g.seed_from_dump(&st, true);
                let d = |g: &Game| g.engine_strip(0).iter().zip(want).filter(|(a, b)| a != b).count();
                checked += 1;
                if d(&g) == 0 {
                    with += 1;
                }
                g.visitor_draw = false;
                if d(&g) == 0 {
                    without += 1;
                }
            }
        }
        assert!(checked > 40, "only {checked} visitor dumps sampled");
        eprintln!("visitor dumps {checked}: host console exact with draw {with}, without {without}");
        assert!(with >= checked / 3, "with the visitor drawn: {with} of {checked}");
        assert!(without * 4 < with * 2, "the draw must close the console: {without} vs {with}");
    }

    /// A spy who steps through a door onto the other spy's screen is re-tested with the wider
    /// room-entry window (`map_x <= x <= map_x + 0x2C`) and takes his own scroll a viewport away.
    #[test]
    fn a_door_step_retests_the_visitor_with_the_entry_window() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(80),
                                     ..RoundOpts::default() });
        g.spies[0].screen = 4;
        g.spies[0].x3 = 150 * SUB;
        g.spies[0].map_x = 130;
        g.spies[1].screen = 4;
        g.spies[1].x3 = 132 * SUB;
        g.spies[1].map_x = 112;
        g.spies[1].blank = false;
        g.spies[0].blank = false;
        g.after_room_step(1); // player 2 lands on screen 4 at x 132, inside 112 .. 112+0x2C
        assert!(g.spies[1].blank, "player 2 is the visitor of player 1's view");
        assert_eq!(g.spies[1].map_x, 130 - VISITOR_STEP_RIGHT);
    }

    // ---- stage 4: the start placement, the terrain and the visitor window -------------

    /// **The start placement** (`FUN_26c0_257b`): `screen = rand & 6`, `x = 26 + rand & 0xff`,
    /// `y = 44`, retried until the terrain code is exactly 0. Over 400 seeded rounds on every
    /// island, every landed position must be on open ground, `x` in the draw's own range, `y` = 44,
    /// and the two spies must be on different screens.
    #[test]
    fn the_round_start_places_both_spies_on_open_ground() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for island in 0..7 {
            let d = IslandData::from_exe(&exe(), island).expect("island");
            for seed in 0..60u32 {
                let g = game(RoundOpts { island, players: 2, seed, ..RoundOpts::default() });
                for (i, spy) in g.spies.iter().enumerate() {
                    let code = g.terrain.code(&g.levels, spy.screen, spy.x(), spy.y());
                    assert_eq!(code, Some(terrain::T_OPEN),
                               "island {} seed {seed} player {}: screen {} x {} y {} has code {code:?}",
                               island + 1, i + 1, spy.screen, spy.x(), spy.y());
                    assert_eq!(spy.y(), START_Y, "every recorded round starts at y = 44");
                    assert!((START_X_BASE..=START_X_BASE + 0xFF).contains(&spy.x()),
                            "x = {} is outside the draw's range", spy.x());
                    assert!(spy.screen <= 8, "screen = {} outside rand & 6", spy.screen);
                }
                assert_ne!(g.spies[0].screen, g.spies[1].screen,
                           "island {} seed {seed}: both spies on screen {}", island + 1, g.spies[0].screen);
                assert_eq!(d.levels.len(), 10);
            }
        }
    }

    /// The old default — `x = 29` on the island's first start screen — is the **uninitialised
    /// data-segment image** (`DS:0x329B` = 29, `0x329D` = 92), and on island 1 it stands on the
    /// level-8 band. The placement must never produce it as a *drawn* result on this island.
    #[test]
    fn the_stale_image_position_is_not_a_start() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = IslandData::from_exe(&exe(), 0).expect("island 1");
        let t = Terrain::from_exe(&exe()).expect("terrain");
        assert_eq!(d.start_screen_p1, 4);
        assert_eq!(t.code(&d.levels, d.start_screen_p1, 29, START_Y), Some(terrain::T_CLIMB),
                   "the data image's x = 29 is the level-8 band, not open ground");
        assert!(!t.start_ok(&d.levels, d.start_screen_p1, 29, START_Y));
        // and the engine's own placement never lands there
        for seed in 0..200u32 {
            let g = game(RoundOpts { seed, ..RoundOpts::default() });
            assert!(!(g.spies[0].screen == 4 && g.spies[0].x() == 29 && g.spies[0].y() == START_Y),
                    "seed {seed} drew the stale image position");
        }
    }

    /// `--void-solid` turns the level-8 band (mask code 1, a *climb*) into a wall. Off by default
    /// because the original walks on it; on, the spy cannot leave the room's own cells.
    #[test]
    fn the_void_solid_flag_makes_the_ocean_a_wall() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // screen 6's room has level 0 at column 6, the level-4 rock at column 7 and the level-8
        // band from column 8; at y = 36 the rock's mask is open, so a spy can walk east into it
        let opts = RoundOpts { start_screen: Some(6), start_x: Some(170), start_y: Some(36),
                               ..RoundOpts::default() };
        let mut open = game(opts.clone());
        assert_eq!(open.levels[3][8], 8, "column 8 of screen 6 is the band");
        assert_eq!(open.terrain.code(&open.levels, 6, 210, 36), Some(terrain::T_CLIMB));
        // 30 frames (stage 16a: 26 ticks each; this was 180 ticks of a 60 Hz tick)
        hold(&mut open, RIGHT, 30 * FRAME_TICKS as usize);
        assert!(open.spies[0].x() > 210, "the original walks on into the band: {}", open.spies[0].x());

        let mut walled = game(RoundOpts { void_solid: true, ..opts });
        hold(&mut walled, RIGHT, 30 * FRAME_TICKS as usize);
        // the last open unit is 202; 203..207 are the rock's climb cells, then the band
        assert_eq!(walled.spies[0].x(), 202, "with the flag the climb cells and band refuse");
    }

    /// **The engine's own predicate against the corpus.** Walk a spread of recorded dumps (the
    /// dump names come from each session's `state.csv`, every 200th row) and ask
    /// [`Terrain::solid`] about both spies' positions. Every in-round position must be on a
    /// non-solid cell; the only rows that fail are a session's *first* dump, which is the state
    /// before `FUN_26c0_257b` runs (the data image's own P2 x = 92, on the island's start screen).
    #[test]
    fn no_recorded_position_is_on_a_solid_cell() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["playtest1", "playtest2", "playtest3", "playtest4", "playtest5",
                        "trapulator-island4", "m3-heat", "rocket-room-2", "audio-smoke", "iceaxe"];
        let mut checked = 0usize;
        let mut bad: Vec<String> = Vec::new();
        let mut premove = 0usize;
        for ses in sessions {
            let csv = crate::orig::require("recon/round/sessions")
                .join(ses)
                .join("state.csv")
                .display()
                .to_string();
            let Ok(text) = std::fs::read_to_string(&csv) else { continue };
            let header: Vec<&str> = text.lines().next().unwrap_or("").split(',').collect();
            let dump_col = header.iter().position(|h| *h == "dump").unwrap_or(23);
            let mut first = true;
            let mut with_dump = 0usize;
            for line in text.lines().skip(1) {
                let Some(name) = line.split(',').nth(dump_col) else { continue };
                if !name.ends_with(".bin") {
                    continue;
                }
                with_dump += 1;
                if with_dump % 25 != 0 {
                    continue;
                }
                let path = crate::orig::require("recon/round/sessions")
                    .join(ses)
                    .join(name);
                let Ok(st) = round::RoundState::from_ds_dump(&path) else { continue };
                let g = game(RoundOpts::default());
                for i in 0..2usize {
                    let p = &st.players[i];
                    if p.band % 2 != 0 {
                        continue;
                    }
                    checked += 1;
                    if g.terrain.solid(&st.levels, p.band, p.spy.x as i32, p.spy.y as i32) {
                        if first && i == 1 && p.spy.x == 92 {
                            premove += 1; // the pre-placement image row, reported below
                        } else {
                            bad.push(format!("{ses}/{name} player {} at ({}, {}, {})",
                                             i + 1, p.band, p.spy.x, p.spy.y));
                        }
                    }
                }
                first = false;
            }
        }
        assert!(checked > 500, "only {checked} positions sampled");
        assert!(bad.is_empty(), "{} positions on solid cells: {:?}", bad.len(), &bad[..bad.len().min(6)]);
        eprintln!("checked {checked} recorded positions; {premove} of them the pre-placement row");
    }

    // ---- stage 3: the engine's own frame ---------------------------------

    /// The `dump` column of a session's `state.csv`. The captures disagree on their trailing
    /// columns (`kit_p2` is present in some and absent in others), so the column is found by name.
    fn dump_column(text: &str) -> Vec<String> {
        let mut lines = text.lines();
        let head: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
        let ix = head.iter().position(|c| *c == "dump");
        let Some(ix) = ix else { return Vec::new() };
        lines.filter_map(|l| l.split(',').nth(ix).map(|s| s.trim().to_string())).collect()
    }

    fn session_dump(ses: &str, name: &str) -> Option<round::RoundState> {
        let p = crate::orig::require("recon/round/sessions")
            .join(ses)
            .join(name);
        if !p.exists() {
            eprintln!("skip: {} not present", p.display());
            return None;
        }
        Some(round::RoundState::from_ds_dump(&p).expect("parse dump"))
    }

    /// The round-start object layout, replayed **exactly** against the recorded attract round.
    ///
    /// The attract round is the only recorded round whose PRNG input is fixed: `DS:0x3203` is
    /// non-zero, so `FUN_26c0_20cd` takes `DX = 0x770D` instead of the DOS clock, and every draw is
    /// `((state_lo + 0x77) * 0x0D) & 0xFFFF`. That is why `m1-fix-960/ds_00004.bin` and
    /// `playtest3/ds_00247.bin` — two different sessions, two different runs — hold the *same* 14
    /// objects in the same 14 slots; it is also the only round in the corpus whose layout can be
    /// replayed. Normal rounds take `dh`/`dl` from `int 21h ah=2Ch` at every draw, so their exact
    /// layouts are not recoverable from a dump (their *rules* are, and the rules are what the port
    /// implements).
    ///
    /// The state that reproduces the recording is `0x0007` (the low byte is all that matters). The
    /// `0x469` the attract branch writes is not the first-draw state: something draws between the
    /// write and the placement.
    #[test]
    fn the_attract_round_layout_replays_object_for_object() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(dump) = session_dump("m1-fix-960", "ds_00004.bin") else { return };
        let g = game(RoundOpts { island: 0, players: 2, obj_state: Some(0x0007),
                                 ..RoundOpts::default() });
        assert_eq!(g.layout.objects.len(), 14, "island 1 places 14 objects");
        let mut hits = 0;
        for o in &g.layout.objects {
            let base = u16::from_le_bytes([dump.ds[round::DS_BLOCK_PTR + 2 * (o.band / 2) as usize],
                                           dump.ds[round::DS_BLOCK_PTR + 2 * (o.band / 2) as usize + 1]])
                as usize;
            let want = dump.ds[base + o.row * round::OBJ_ROW_LEN + o.entry];
            assert_eq!(want, o.code,
                       "band {} row {} entry {}: recorded {want:#04x}, placed {:#04x}",
                       o.band, o.row, o.entry, o.code);
            hits += 1;
        }
        assert_eq!(hits, 14, "every placed object must be in the recorded slot");
        // and the recorded block holds nothing else in rows 1..3 but these objects and the hatch
        let mut extra = 0;
        for band in [0u16, 2, 4, 6, 8] {
            let base = u16::from_le_bytes([dump.ds[round::DS_BLOCK_PTR + 2 * (band / 2) as usize],
                                           dump.ds[round::DS_BLOCK_PTR + 2 * (band / 2) as usize + 1]])
                as usize;
            for row in 1..4usize {
                for entry in 0..round::OBJ_ROW_LEN {
                    let v = dump.ds[base + row * round::OBJ_ROW_LEN + entry];
                    let placed = g.layout.objects.iter().any(|o| {
                        o.band == band && o.row == row && o.entry == entry
                    });
                    if v != 0 && !placed {
                        // the hatch's three bytes are the only other thing the round start writes
                        let hatch = v == round::HATCH_CLOSED || v == round::HATCH_FLANK;
                        assert!(hatch, "unexplained byte {v:#04x} at band {band} row {row} entry {entry}");
                        extra += 1;
                    }
                }
            }
        }
        assert_eq!(extra, 3, "the hatch's three bytes");
    }

    // ---- stage 9a: the launch site's hatch, the polar bear, the igloo fires -------------------

    /// `CS:2696` + `CS:0610` — **the hatch is not in the level at all while the launch site is on
    /// HIDE**; it appears on the frame the deposited mask reaches `0x6C0` (the briefcase bit plus
    /// the three parts), and its three entries are then `15 0F 15`.
    ///
    /// Fails against the stage-8 build, which wrote the three bytes at placement whatever the
    /// option was — the owner's "hatch visible from the start even with the launch site on HIDE".
    #[test]
    fn the_hatch_stays_hidden_until_the_briefcase_holds_the_three_parts() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: false,
                                     ..RoundOpts::default() });
        let hatch = g.hatch.addr.expect("the placement still picks a cell");
        assert_eq!(g.hatch.armed, 0, "HIDE: 0x31D5 is 0");
        assert_eq!(&g.ds[hatch - 1..hatch + 2], &[0, 0, 0],
                   "a HIDE round leaves the hatch's three entries empty");
        // the strip has no hatch ink: the object layer only draws what is in the block
        let before = g.engine_strip(0);
        // two parts is not enough
        g.mask = round::MASK_HATCH_REVEAL & !0x200;
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        assert_eq!(&g.ds[hatch - 1..hatch + 2], &[0, 0, 0], "two parts do not reveal it");
        // the third part lands in the case
        g.mask = round::MASK_HATCH_REVEAL;
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        assert_eq!(&g.ds[hatch - 1..hatch + 2],
                   &[round::HATCH_FLANK, round::HATCH_CLOSED, round::HATCH_FLANK],
                   "the reveal writes 15 0F 15 ({:?})", &g.ds[hatch - 1..hatch + 2]);
        assert_eq!(g.hatch.armed, 2, "the reveal sets 0x31D5 = 2, once");
        let after = g.engine_strip(0);
        let ink = before.iter().zip(after.iter()).filter(|(a, b)| a != b).count();
        assert!(ink > 0, "the reveal must change the composed strip (the hatch is drawn)");
        // and it happens once: another frame with the same mask changes nothing
        let again = g.engine_strip(0);
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        assert_eq!(g.engine_strip(0), again, "the reveal is not repeated");
    }

    /// A **SHOW** round inks the hatch at placement (`CS:269d`), so the same three bytes are there
    /// before a single frame runs.
    #[test]
    fn a_show_round_has_the_hatch_from_the_first_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = game(RoundOpts { island: 0, players: 2, launch: true, ..RoundOpts::default() });
        let hatch = g.hatch.addr.expect("a cell was picked");
        assert_eq!(g.hatch.armed, 2);
        assert_eq!(&g.ds[hatch - 1..hatch + 2],
                   &[round::HATCH_FLANK, round::HATCH_CLOSED, round::HATCH_FLANK]);
    }

    /// `FUN_26c0_2392` + the animation machine: walking onto the hatch opens it (`0x10`), the
    /// descent runs 16 frames, and **ten frames into state `0x20` the polar bear is written into
    /// the entry two to the right of the hatch's middle** (`CS:23b7`), saving what it replaced.
    /// The close state then puts `0x0F` back and restores that byte.
    ///
    /// The timings are the executable's own: `word[0x2CE6 + 0x1E] = 30` and
    /// `word[0x2CE6 + 0x20] = 30` are *frame ceilings* (`0x2EFC` steps by 2), so each state lasts
    /// 16 frames; `0x1C`'s ceiling of 16 makes the close 9 frames.
    #[test]
    fn the_bear_comes_out_ten_frames_into_the_second_state() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        let hatch = g.hatch.addr.expect("hatch");
        assert_eq!(g.spies[0].screen, g.layout.hatch.as_ref().unwrap().band);
        let frames = |g: &mut Game, n: usize| {
            for _ in 0..n {
                hold(g, NOKEYS, FRAME_TICKS as usize);
            }
        };
        // frame 0: the spy is standing on the closed hatch; the first frame opens it
        frames(&mut g, 1);
        assert_eq!(g.hatch.seq, HatchSeq::Descend, "the touch starts the descent");
        assert_eq!(g.ds[hatch], round::HATCH_OPEN, "the hatch reads 0x10 while it is open");
        // 16 frames of descent (script entries 0..15), then the end test fires on the 17th frame
        // of the state — `0x2EFC` is `2f - 2`, so `2f > 30` at f = 16. Stage 9a stopped one frame
        // early here, which cost the throw its last `vy = +2` (see `STAGE10_REPORT.md` §1).
        frames(&mut g, 16);
        assert_eq!(g.hatch.seq, HatchSeq::Bear, "state 0x1E plays 16 script frames");
        assert_eq!(g.hatch.bear_frames, 0);
        assert_eq!(g.ds[hatch + 2], 0, "no bear yet");
        // ten frames into 0x20 the bear appears at [0x31B6 + 2]
        frames(&mut g, 9);
        assert!(!g.hatch.bear, "nine frames is not enough");
        frames(&mut g, 1);
        assert_eq!(g.hatch.bear_frames, 10);
        assert!(g.hatch.bear, "ten frames in, the bear is out");
        assert_eq!(g.ds[hatch + 2], round::BEAR, "[0x31B6+2] holds 0x11");
        assert_eq!(g.hatch.saved, 0, "the byte it replaced is saved in 0x31B8");
        // the spy has moved with the animation script: +1 x per frame from frame 7 of 0x20
        assert!(g.spies[0].x() > 2 * 83 + 2, "the descent walks the spy right (x={})", g.spies[0].x());
        // the close state: hatch 0x0F again and the saved byte back. `0x20` also plays 16 script
        // frames, so its end test fires on the 17th advance of the state; `0x1C` has no script (it
        // is below 0x1E, `CS:09c7`) and plays its 9 frames from `word[0x2D22 + 0x2EFC]`, the walk
        // cycle, before the close fires on the 10th.
        frames(&mut g, 7);
        assert_eq!(g.hatch.seq, HatchSeq::Close, "state 0x20 plays 16 script frames");
        frames(&mut g, 10);
        assert_eq!(g.hatch.seq, HatchSeq::Idle, "state 0x1C plays 9 frames");
        assert_eq!(g.ds[hatch], round::HATCH_CLOSED, "the hatch closes");
        assert_eq!(g.ds[hatch + 2], 0, "and the bear's byte is restored");
        assert!(!g.hatch.bear);
        assert_eq!(g.hatch.bear_frames, 0, "0x2F74 resets");
    }

    /// `CS:284d` — the descent's first state only ends in the bear when this spy's **kit** is
    /// missing a part. All three parts in hand (`kit & 0x2C0 == 0x2C0`, the *per-spy* word, not the
    /// shared mask) and the hatch launches instead.
    #[test]
    fn all_three_parts_in_hand_launch_instead_of_the_bear() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        let hatch = g.hatch.addr.expect("hatch");
        g.spies[0].kit |= round::KIT_ALL_PARTS;
        for _ in 0..20 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.seq, HatchSeq::Launched, "the rocket branch, not the bear");
        assert_eq!(g.ds[hatch + 2], 0, "no bear byte in the block");
        assert!(!g.hatch.bear);
        // The recorded words after a launch (`playtest4` ds_01814): `0x2F70` = 1 on the launcher,
        // `0x2F64` = 2 and `0x2F7E` = 1 on the other spy. The launcher is **not** left standing
        // half-sunk in the hatch: `0x2F18` is cleared and `0xc93` never draws him again (it draws
        // the rocket instead, which is not ported), so the owner's "begins the first frame of
        // falling then stops, the other spy can still move" is gone.
        assert!(g.rocket, "0x2F70: the rocket is in flight");
        assert!(g.spies[0].launched, "and the launcher is out of the player loop");
        assert!(!g.spies[0].pose_raised, "he is not left in the descent pose");
        assert!(g.spies[1].out_of_round, "0x2F64(other) = 2");
        assert!(g.spies[1].blank, "0x2F7E(other) = 1: the other spy is a visitor");
        // and the spy layer really skips him — not through `0x2F64`, which the recording keeps at
        // **0** for a launcher, but through `0x2F70`, which is what `0xc93` tests first (`CS:cc5`)
        let st = g.round_state();
        assert_eq!(u16::from_le_bytes([st.ds[round::DS_SPY_STATE],
                                       st.ds[round::DS_SPY_STATE + 1]]), 0,
                   "0x2F64(launcher) stays 0 — the recorded word (20 of 20 launch dumps)");
        assert_eq!(u16::from_le_bytes([st.ds[0x2F70], st.ds[0x2F71]]), 1,
                   "0x2F70 = 1 is what draws the rocket instead of him (`CS:cc5`)");
    }

    /// **A — the descent slides, it does not walk.** `0x2F00`/`0x2F04` (the velocities the script
    /// writes) are **zero for all sixteen frames** of state `0x1E`; the only thing that changes is
    /// the third column of the script, `0x2F68`, which runs 2, 4, .., 28, 28, 28 and is what the
    /// blitter uses to sink the sprite into the hatch mouth. So the spy's `x`/`y` are frozen and
    /// the legs are a fixed record (`CS:e3e`: raised and not `0x1C` → `+2`), which is the owner's
    /// "the legs do not appear to move and the spy smoothly slides down into the hatch".
    ///
    /// Recorded reference: `playtest4` ds_01774..ds_01783 — x 116 and y 54 for the whole descent,
    /// clip 2 at the entry dump and 28 at the last one.
    #[test]
    fn the_descent_sinks_the_spy_with_the_clip_and_never_moves_him() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        assert_eq!(g.hatch.seq, HatchSeq::Descend);
        let (x0, y0) = (g.spies[0].x(), g.spies[0].y());
        assert_eq!(y0, 0x36, "`CS:2436`: y = 0x36 = 54");
        // the touch frame has already run script entry 0, so the clip starts at 2
        let mut clips = vec![g.spies[0].pose_clip];
        let mut legs = Vec::new();
        for _ in 0..15 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
            clips.push(g.spies[0].pose_clip);
            legs.push(pose_of(&g, 0).records(&g.ds_img()).1);
        }
        assert_eq!((g.spies[0].x(), g.spies[0].y()), (x0, y0), "the descent never moves him");
        assert_eq!(clips, vec![2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 28, 28],
                   "the script's third column, frame for frame");
        assert!(legs.windows(2).all(|w| w[0] == w[1]), "one fixed legs record: no walk cycle");
        // the legs are gone once the clip passes the six rows of clearance (`CS:dc9`), and the body
        // is pinned at y with its bottom rows cut once the clip passes the body's own `sub $9`
        let p = pose_of(&g, 0);
        assert!(!p.legs_drawn(), "clip 28 > 6: no legs");
        assert_eq!(p.body_place(y0), (y0, 28 - round::BODY_CLIP), "bottom pinned, rows cut");
    }

    /// **A — the arc.** State `0x20`'s script (`0x2C60`) is `(vy, vx)` per frame, and its last entry
    /// is `vy = +2`: `0x1E` ends, sixteen frames of `0x20` follow, and the spy lands **+7 east and
    /// −2 north** of where he went down. Stage 9a dropped each state's final script frame, so the
    /// port landed two units short — the owner's "landing a little north".
    ///
    /// The expectation is the recording's own trace, normalised to the entry x:
    /// `playtest4` ds_01774..01783, x 116 -> 123, y 54,50,48,46,44,44,48,50,52.
    #[test]
    fn the_throw_arc_lands_on_the_recorded_square() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        let x0 = g.spies[0].x();
        // 16 frames of descent, then 16 of the throw
        for _ in 0..16 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.seq, HatchSeq::Bear);
        let mut trace = Vec::new();
        for _ in 0..16 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
            trace.push((g.spies[0].x() - x0, g.spies[0].y()));
        }
        assert_eq!(trace, vec![
            (0, 54), (0, 54), (0, 54), (0, 54), (0, 54), (0, 54), (0, 54),
            (0, 50), (1, 48), (2, 46), (3, 44), (4, 44), (5, 46), (6, 48), (7, 50), (7, 52),
        ], "the recorded arc: up 4, then up 2 a frame while drifting east, then down 2");
        assert_eq!(g.spies[0].x() - x0, 7, "seven units east of the hatch");
        assert_eq!(g.spies[0].y(), 52, "and two north of where he went down");
    }

    /// **A — the walk at the landing.** State `0x1C` has no script (`CS:09c2` sends any state below
    /// `0x1E` straight to the commit), so the spy stands still; the **legs** come from
    /// `word[0x2D22 + 0x2EFC]` (`CS:e2d`), which is `0, 2, 4, 2, 0, 2, 4, 2, 0` — the walk cycle.
    /// The body does not advance (state `>= 0x1C` never adds the frame), so he stands facing east
    /// and walks his legs on the spot: the owner's "just as they land the legs do the walk
    /// animation". Stage 9a drew `+2` for every raised state, so the legs never moved.
    #[test]
    fn the_climb_out_walks_the_legs_while_the_body_stands_still() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        for _ in 0..34 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.seq, HatchSeq::Close);
        // the state's entry frame still carries `0x2EFC = -2` (`2807` jumps past the commit), so
        // start with the first frame the counter has advanced on
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        let mut bodies = Vec::new();
        let mut legs = Vec::new();
        let (x0, y0) = (g.spies[0].x(), g.spies[0].y());
        for _ in 0..9 {
            let p = pose_of(&g, 0);
            let (b, l) = p.records(&g.ds_img());
            bodies.push(b);
            legs.push(l);
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!((g.spies[0].x(), g.spies[0].y()), (x0, y0), "the climb-out stands still");
        assert!(bodies.windows(2).all(|w| w[0] == w[1]), "one body record: he is standing");
        assert_eq!(legs.len(), 9);
        assert_eq!(legs[0], legs[4], "the cycle comes round at frame 4");
        assert_eq!(legs[0], legs[8], "and again at frame 8");
        assert_ne!(legs[0], legs[1], "the legs alternate");
        assert_eq!(legs.iter().collect::<std::collections::BTreeSet<_>>().len(), 3,
                   "three leg records: the walk cycle's 0, 2 and 4");
        let p = pose_of(&g, 0);
        assert_eq!(p.dir, 0, "facing east (`CS:27e0` set 0 for the throw; 0x1C keeps it)");
    }

    /// **The briefcase under the thrown spy.** `FUN_26c0_277c` seeds `0x2EFC = -2` when a state
    /// starts and `FUN_26c0_2807` returns into `LAB_26c0_0a06`, the frame step, so the frame that is
    /// *drawn* on the entry tick is `0` — `-2` never reaches the pose tables. The port drew the
    /// seed frame, and `0x1C`'s legs at `-2` read `word[0x2D22 - 2] = word[0x2D20] = 0x6C`: the
    /// **carrying** pose block, which paints a briefcase at the spy's feet for the entry tick (two
    /// 60 Hz ticks on screen) — the owner's post-throw glitch, which the original does not show.
    #[test]
    fn the_climb_out_does_not_draw_the_carried_set_under_the_spy() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        let mut guard = 0;
        while g.hatch.seq != HatchSeq::Close && guard < 200 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
            guard += 1;
        }
        assert_eq!(g.hatch.seq, HatchSeq::Close, "the throw reached its climb-out");
        let i = g.hatch.spy;
        assert_eq!(g.spies[i].pose_state, round::STATE_CLOSE);
        let p = pose_of(&g, i);
        assert_eq!(p.frame, round::SEED_FRAME, "the state machine still steps from -2");
        let ds = g.ds_img();
        let entry = |step: usize| -> u16 {
            let off = p.extra as usize + p.dir as usize + step;
            let v = u16::from_le_bytes([ds[round::DS_POSE_TABLE + off], ds[round::DS_POSE_TABLE + off + 1]]);
            round::SPRITE_BASE.wrapping_add(v)
        };
        let (_, legs) = p.records(&ds);
        eprintln!("entry-tick legs {legs} (frame 0 -> {}), seed-frame legs {} (0x6C carry block); legs_drawn {} clip {} raised {} state {}",
                  entry(0), entry(0x6C), p.legs_drawn(), p.clip, p.raised, p.state);
        assert_eq!(legs, entry(0), "the entry tick draws the walk cycle's first leg record");
        assert_ne!(legs, entry(0x6C),
                   "and NOT `word[0x2D22 - 2] = 0x6C`, the carrying block (the briefcase)");
        // the entry tick draws frame 0 and the next tick also draws 0 (the advance lands on it), so
        // the cycle next steps on the tick after that — `0x2D22` = 0, 2, 4, 2, ...
        hold(&mut g, NOKEYS, 2 * FRAME_TICKS as usize);
        let (_, legs1) = pose_of(&g, i).records(&ds);
        assert_ne!(legs1, legs, "the walk cycle starts stepping");
    }

    /// **A — the other spy laughs.** `CS:2915` starts state `0x0C` on him through `277c`, and that
    /// routine's `CS:27e9` path switches his pose set to `word[0x2CC4 + 0x0C]` = **`0xDC`**, with
    /// the direction 0 and the frame back to `-2`. Measured on `playtest4` ds_01783 — the laugh's
    /// first frame — with the set already `0xDC` and `0x2EF8` back to `0x24` ten frames later.
    /// Stage 9a left the set at `0x24`, so the other spy was drawn as an ordinary walker while the
    /// thrown spy flew: the owner's "the black spy does a LAUGHING animation, not the falling-down
    /// animation the port is doing".
    #[test]
    fn the_other_spy_laughs_with_the_dc_pose_set() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        for _ in 0..34 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.other, Some(1), "the other spy is in 0x0C");
        let laugh = round::anim_set(&g.ds_img(), 0x0C);
        assert_eq!(laugh, 0xDC, "`word[0x2CC4 + 0x0C]`, read out of the executable");
        let p = pose_of(&g, 1);
        assert_eq!(p.set, laugh, "the set the animation switches to");
        assert_eq!(p.dir, 0, "facing horizontally");
        assert!(p.raised && p.state == 0x0C);
        assert!(p.body_takes_frame(), "his body is what animates");
        assert!(!p.legs_drawn(), "and no legs overlay on this branch (`CS:da5`)");
        // the body record actually changes frame to frame: the laugh is an animation, not a pose
        let mut bodies = Vec::new();
        for _ in 0..4 {
            bodies.push(pose_of(&g, 1).records(&g.ds_img()).0);
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert!(bodies.windows(2).any(|w| w[0] != w[1]), "the body animates: {bodies:?}");
        // ten frames (`word[0x2CE6 + 0x0C]` = 18, `2f > 18`) and he is back to his own pose
        for _ in 0..8 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.other, None, "0x0C is over");
        let p = pose_of(&g, 1);
        assert_eq!(p.set, 0x24, "`CS:280d` restores the saved set");
        assert!(!p.raised, "and 0x2F18 clears");
        assert_eq!(p.frame, STAND_FRAME, "frame back to the standing one");
    }

    /// **A — the animation scrolls the spy's own view.** `FUN_26c0_0000` is on the frame's tail
    /// (`CS:0a3e`) for every player, raised or not; only the frame-step hold is skipped for a raised
    /// one (`CS:09b`, `CS:130`). Measured on the recorded throw: `playtest4` ds_01775..1781 — his x
    /// 116 -> 123 while his `map_x` goes **82 -> 94**, 2 a frame, and it stops dead at 94 when the
    /// arc stops moving him (ds_01782..1786 all read 94).
    #[test]
    fn the_animation_scrolls_the_thrown_spys_own_view() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        for _ in 0..17 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.seq, HatchSeq::Bear, "the throw's first frame");
        // the recorded entry has him 34 units past his own scroll (he walked into the hatch at the
        // dead zone and `CS:2436` put him on the hatch's own column), so the scroll keeps firing
        let x0 = g.spies[0].x();
        g.spies[0].map_x = x0 - 34;
        let map0 = g.spies[0].map_x;
        let mut maps = Vec::new();
        for _ in 0..16 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
            maps.push(g.spies[0].map_x - map0);
        }
        // no key is held: the only thing arming the scroll is the script's own x velocity, so the
        // view follows the arc and stops dead when the arc stops (`vx = 0` on frames 0..7 and 15).
        // The recording: `playtest4` ds_01775..1781, his x 116 -> 123 with `map_x` 82 -> 94.
        assert_eq!(maps, vec![0, 0, 0, 0, 0, 0, 0, 0, 2, 4, 6, 8, 10, 10, 12, 12],
                   "the script's vx is what arms the scroll (`CS:9ea` writes the right-key flag)");
        assert_eq!(maps[15], 12, "twelve units of scroll against seven of x, as recorded");
        assert_eq!(g.spies[0].x() - x0, 7, "and the arc itself is unchanged");
        assert_eq!(g.spies[0].x() - g.spies[0].map_x, 29,
                   "he ends 29 from the window's left edge, the recording's 123 - 94");
    }

    /// **B — the west push reaches the host's own cut-off.** `CS:010d..011d` refuses the push when
    /// `host.map + 0x1C < host.x`, i.e. when the host is already 28 units in from his window's left
    /// edge — one 2-unit step short of his own right-hand cut-off (`SCROLL_RIGHT_EDGE` = 30). Stage
    /// 8b wrote that constant as `SCROLL_RIGHT_EDGE - 0x10` = **14**, so a west push stopped as
    /// soon as the host reached screen x 14 while an east push ran to x 6: the owner's "if he walks
    /// WEST he stops being able to push ... when the other player has ended up at the half-way
    /// mark".
    ///
    /// The recording lands the host on **30** (`trapulator-island4` ds_02015..02033: his screen x
    /// 2, 4, 6, .. 30 and then nothing) and on **6** for an east push (`item-check-2` 00335..).
    #[test]
    fn a_visitor_pushing_west_takes_the_host_to_his_own_cut_off() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts {
            players: 2,
            start_screen: Some(4),
            start_x: Some(120),
            start_x2: Some(140),
            start_y: Some(52),
            ..RoundOpts::default()
        });
        // host at screen x 6 of his own view; the visitor at the host's west edge, on his row so he
        // can walk past him, and already inside the visitor window
        g.spies[0].map_x = g.spies[0].x() - 6;
        g.spies[1].map_x = g.spies[0].map_x + VISITOR_KEEP_OFFSET;
        g.spies[1].blank = true;
        g.spies[1].y3 = g.spies[0].y3 + 2 * SUB; // one row down: the bodies do not block
        g.spies[1].x3 = (g.spies[0].map_x + 6) * SUB;
        let host_x = g.spies[0].x();
        for _ in 0..40 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
            // the visitor holds left
            for _ in 0..FRAME_TICKS {
                g.tick(Input { p1: NOKEYS, p2: LEFT });
            }
        }
        let screen = host_x - g.spies[0].map_x;
        assert_eq!(screen, SCROLL_RIGHT_EDGE,
                   "the host is pushed to his own cut-off (30), not to 16 (the stage-8 build)");
    }

    /// **The acceptance test for the igloo fires.** `FUN_26c0_257b`'s tail is driven from the
    /// PRNG state the *recording* proves: from state `0xB39` (only the low byte matters) the loop
    /// must produce the attract round's own interior rows — `(7,6) 0x12 -> 0x16`, `(8,3)` left
    /// bare — and `DS:0x2FB0 = 1`, and must leave the PRNG in the recorded `0x090A`.
    ///
    /// The port's own placement stream reaches the lottery in a different state (the spy draws
    /// part company there; see the report's open items), so this test seeds the state explicitly,
    /// the same way `the_attract_round_layout_replays_object_for_object` seeds `--obj-state`.
    #[test]
    fn the_attract_rounds_igloo_fires_replay_from_the_recorded_stream() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(dump) = session_dump("m1-fix-960", "ds_00004.bin") else { return };
        let data = IslandData::from_exe(&exe(), 0).expect("island 1");
        let mut levels = data.levels;
        // the island's own table: two bare interiors, (7,6) and (8,3)
        assert_eq!(levels[7][6], round::LEVEL_INTERIOR_BARE);
        assert_eq!(levels[8][3], round::LEVEL_INTERIOR_BARE);
        let mut rng = round::ObjRng::at(0xB39);
        let fires = round::place_igloo_fires(&mut levels, &mut rng);
        assert_eq!(fires, 1, "one interior got a fire");
        assert_eq!(fires, dump.ds[round::DS_FIRES] as u16, "DS:0x2FB0 in the recording");
        for r in 5..10 {
            for c in 0..12 {
                let want = u16::from_le_bytes([dump.ds[round::DS_LEVELS + 24 * r + 2 * c],
                                               dump.ds[round::DS_LEVELS + 24 * r + 2 * c + 1]]);
                assert_eq!(levels[r][c], want,
                           "row {r} col {c}: recorded {want}, lottery {}", levels[r][c]);
            }
        }
        assert_eq!(levels[7][6], round::LEVEL_INTERIOR_FIRE, "(7,6) got the fire");
        assert_eq!(levels[8][3], round::LEVEL_INTERIOR_BARE, "(8,3) was declined");
        assert_eq!(rng.state(), 0x090A, "the PRNG ends where the recording's dump says");
    }

    /// The fire is the level-`0x16` interior plate, and `DS:0x2FB0` counts exactly those cells:
    /// the identity holds in **19,034 of 19,034** recorded dumps (the probe prints the same
    /// number). A sample of the corpus is asserted here so the mapping cannot drift unnoticed.
    #[test]
    fn the_fire_count_is_the_number_of_flaming_igloo_interiors() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let dir = crate::orig::require("recon/round/sessions");
        let mut checked = 0usize;
        for sess in ["m1-fix-960", "playtest4", "audio-smoke"] {
            for name in ["ds_00004.bin", "ds_00383.bin", "ds_00100.bin"] {
                let p = dir.join(sess).join(name);
                let Ok(bytes) = std::fs::read(&p) else { continue };
                let st = round::RoundState::from_bytes(bytes).expect("dump");
                let n = (5..10)
                    .flat_map(|r| (0..12).map(move |c| (r, c)))
                    .filter(|&(r, c)| st.levels[r][c] == round::LEVEL_INTERIOR_FIRE)
                    .count() as u16;
                let fires = u16::from_le_bytes([st.ds[round::DS_FIRES], st.ds[round::DS_FIRES + 1]]);
                assert_eq!(n, fires, "{sess}/{name}: 0x2FB0 must count the 0x16 interiors");
                checked += 1;
            }
        }
        assert!(checked >= 3, "the corpus must be present ({checked} dumps read)");
    }
    /// The best of the two consoles, because `DS:0x1A92` holds whichever was drawn last.
    fn best_residual(g: &Game, want: &[u8]) -> usize {
        (0..2)
            .map(|p| g.engine_strip(p).iter().zip(want).filter(|(a, b)| a != b).count())
            .min()
            .unwrap()
    }

    /// **The acceptance test for the engine path.** Seed the engine with a recorded dump's state and
    /// the engine must compose the strip the real game left in that dump, byte for byte — the same
    /// contract the dump-driven path meets (stage 2A: 881 of 1082). The dumps below are chosen to
    /// cover the cases that broke: an interior (screen 14, row 7 of the level table), band 18 (past
    /// the row-base table), a row-shift pose (`0x2F2C == 7`), a blanked visitor console, and two
    /// ordinary snowfields from different sessions.
    #[test]
    fn engine_reproduces_the_recorded_strip() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for (ses, name) in [
            ("playtest1", "ds_00015.bin"),
            ("playtest2", "ds_00210.bin"),   // screen 14: interior, needs the 10-row level table
            ("playtest4", "ds_00385.bin"),   // band 18: past the row-base table
            ("playtest4", "ds_00020.bin"),   // pose set 48 with the row shift (0x2F2C == 7)
            ("playtest2", "ds_00605.bin"),   // 0x2F7E set: the visitor's console is blanked
            ("rocket-room-2", "ds_00060.bin"),
        ] {
            let Some(st) = session_dump(ses, name) else { continue };
            let mut g = plain();
            g.seed_from_dump(&st, true);
            let want = &st.ds[round::DS_STRIP..round::DS_STRIP + STRIP_LEN];
            assert_eq!(best_residual(&g, want), 0, "{ses}/{name}: engine strip != the dump's");
        }
    }

    /// Without the dump's object blocks the engine still composes the room and the spy, and the only
    /// difference is the items the real round had placed — i.e. the residual is inside the object
    /// boxes and small, not a decode fault.
    #[test]
    fn engine_without_lifted_blocks_differs_only_by_objects() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(st) = session_dump("playtest1", "ds_00015.bin") else { return };
        let mut g = plain();
        g.seed_from_dump(&st, false);
        let want = &st.ds[round::DS_STRIP..round::DS_STRIP + STRIP_LEN];
        let resid = best_residual(&g, want);
        assert!(resid > 0, "this dump has placed objects");
        assert!(resid < 200, "residual {resid} is too large to be the object layer");
        // the plate passes are identical either way
        let mut lifted = plain();
        lifted.seed_from_dump(&st, true);
        assert_eq!(lifted.engine_passes(0).len(), g.engine_passes(0).len());
    }

    /// The round start must clear the exe image's `0x02` object-block filler (72 spurious objects,
    /// which is what the live view drew as a black-and-white fence over the snow), place the
    /// island's objects (`round::place_round_objects`) and set the spy banks, which the image
    /// leaves at zero so no sprite can be drawn at all.
    #[test]
    fn round_start_materialises_the_runtime_fields() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = plain();
        let ds = &g.ds;
        let mut codes = Vec::new();
        for i in 0..5usize {
            let base = u16::from_le_bytes([ds[round::DS_BLOCK_PTR + 2 * i],
                                           ds[round::DS_BLOCK_PTR + 2 * i + 1]]) as usize;
            let blk = &ds[base..base + 624];
            assert!(!blk.iter().any(|&b| b == 0x02), "block {i} still holds the 0x02 filler");
            // the placement only writes rows 1..3; row 0 stays clear
            assert!(blk[..round::OBJ_ROW_LEN].iter().all(|&b| b == 0),
                    "block {i} row 0 must stay clear");
            codes.extend(blk.iter().copied().filter(|&b| b != 0));
        }
        // island 1 is a 14-object island: 3 visible dynamite sticks and 11 mounds
        assert_eq!(g.layout.objects.len(), 14, "island 1's object count");
        assert_eq!(codes.iter().filter(|&&b| b == 0x08).count(), 3, "three visible dynamite");
        // **the launch site defaults to HIDE**, so the hatch is *not* in the block at round start:
        // the placement's write is gated on `DS:0x31D5` (`CS:2696`), and the round loop only puts
        // it there once the briefcase holds all three parts (`CS:0610`). A SHOW round inks it.
        assert!(!codes.contains(&round::HATCH_CLOSED),
                "a HIDE round must not have a hatch on the map");
        assert_eq!(g.hatch.armed, 0, "HIDE: 0x31D5 stays 0 until the reveal");
        let shown = game(RoundOpts { launch: true, ..RoundOpts::default() });
        let ds = &shown.ds;
        let mut codes_show = Vec::new();
        for i in 0..5usize {
            let base = u16::from_le_bytes([ds[round::DS_BLOCK_PTR + 2 * i],
                                           ds[round::DS_BLOCK_PTR + 2 * i + 1]]) as usize;
            codes_show.extend(ds[base..base + 624].iter().copied().filter(|&b| b != 0));
        }
        assert!(codes_show.contains(&round::HATCH_CLOSED), "a SHOW round inks the hatch");
        assert_eq!(shown.hatch.armed, 2, "SHOW: 0x31D5 = 2 at placement");
        for part in [0x89u8, 0x8a, 0x8b, 0x8c] {
            assert!(codes.contains(&part), "rocket part / briefcase {part:#04x} under a mound");
        }
        assert_eq!(u16::from_le_bytes([ds[round::DS_SPY_BANK], ds[round::DS_SPY_BANK + 1]]),
                   round::SPY_BANK_SEG_WHITE);
        assert_eq!(u16::from_le_bytes([ds[round::DS_SPY_BANK + 2], ds[round::DS_SPY_BANK + 3]]),
                   round::SPY_BANK_SEG_BLACK);
        // and the pose block the sprite layer indexes with
        let p1 = round::read_pose(ds, 0);
        let p2 = round::read_pose(ds, 1);
        assert_eq!((p1.set, p1.frame, p1.extra), (0x24, 2, 0));
        assert_eq!((p2.set, p2.frame, p2.extra), (0x24, 2, 0x2EC));
    }

    /// The engine draws the **real** spy sprite, not the marker: with the marker off, the strip
    /// differs from the plates-only composition inside the spy's 8-byte box and nowhere else.
    #[test]
    fn engine_draws_the_spy_sprite() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = plain();
        let with_spy = g.strip_for(0, false);
        // plates + objects but no spy: the object layer is *also* on in `g`, so it must be too here
        // or its pixels would all count as "outside the spy's box".
        let mut plates = plain();
        plates.spies_layer = false;
        let bare = plates.strip_for(0, false);
        let s = &g.spies[0];
        let sx = s.x() - s.map_x;
        let mut outside = 0;
        let mut inside = 0;
        for i in 0..STRIP_LEN {
            if with_spy[i] == bare[i] {
                continue;
            }
            let (r, c) = ((i / STRIP_PITCH) as i32, (i % STRIP_PITCH) as i32);
            if c >= sx && c < sx + 8 && r >= s.y() - 28 && r < s.y() {
                inside += 1;
            } else {
                outside += 1;
            }
        }
        assert!(inside > 20, "the sprite must paint inside its box (got {inside} bytes)");
        assert_eq!(outside, 0, "and nothing outside it");
    }

    /// A one-player round still shows both consoles; the idle one is flat cyan (measured on
    /// `m3-heat`: the `0x2F7E` blanking branch in the bottom viewport, chrome unchanged).
    #[test]
    fn one_player_round_blanks_the_second_console() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = plain();
        assert_eq!(g.players, 1);
        assert!(g.spies[1].blank, "the second console is the blanked one");
        assert!(g.strip_for(1, false).iter().all(|&b| b == 0x55));
        let frame = g.frame(false);
        let (vx, vy) = round::VIEW_BOTTOM;
        let mut uniq = std::collections::BTreeSet::new();
        for row in 0..STRIP_ROWS {
            for col in 0..176 {
                uniq.insert(frame.idx[(vy + row) * crate::W + vx + col]);
            }
        }
        assert_eq!(uniq.into_iter().collect::<Vec<_>>(), vec![1], "flat cyan viewport");
        // the top console is the room, so it is not flat
        let (vx, vy) = round::VIEW_TOP;
        let mut uniq = std::collections::BTreeSet::new();
        for row in 0..STRIP_ROWS {
            for col in 0..176 {
                uniq.insert(frame.idx[(vy + row) * crate::W + vx + col]);
            }
        }
        assert!(uniq.len() > 2, "the top console shows a room");
    }

    /// **The surround is the player's own `TRAP.PIC`**, and the only pixels the engine rewrites
    /// outside the two viewports are the four widgets — the body-heat gauges, the clock's digits,
    /// the tray dots and the kit boxes (`crate::chrome`, `CS:0x21BD`/`0x229E`/`0x3278`/`0x31C8`).
    ///
    /// This is the "no captured pixels" guarantee: the plate is `decode(TRAP.PIC)` plus the
    /// viewport fill, so a pixel of the console casing that is *not* the file's own byte is a bug,
    /// and a widget pixel outside its rectangle is a bug too.
    #[test]
    fn the_surround_is_trap_pic_and_only_the_widgets_are_drawn() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = plain();
        let Some(plate) = g.assets.plate.as_ref() else {
            crate::orig::skip("the player's TRAP.PIC");
            return;
        };
        assert_eq!(plate.len(), crate::PIXELS);
        let frame = g.frame(false);
        let (vx0, vx1) = (round::VIEW_TOP.0, round::VIEW_TOP.0 + 176);
        for y in 0..crate::H {
            for x in 0..crate::W {
                let in_top = x >= vx0 && x < vx1 && y >= 10 && y < 10 + round::STRIP_ROWS;
                let in_bottom = x >= vx0 && x < vx1 && y >= 118 && y < 118 + round::STRIP_ROWS;
                if in_top || in_bottom {
                    continue;
                }
                if chrome::WIDGET_RECTS
                    .iter()
                    .any(|&(x0, y0, x1, y1)| x >= x0 && x < x1 && y >= y0 && y < y1)
                {
                    continue;
                }
                assert_eq!(
                    frame.idx[y * crate::W + x],
                    plate[y * crate::W + x],
                    "surround pixel ({x},{y}) is not TRAP.PIC"
                );
            }
        }
        // the digits are the game's own glyph records, not this port's fallback font: every pixel
        // of the ten glyphs is black-on-magenta (0 or 2) and no two glyphs are the same
        let glyphs = chrome::digit_glyphs(&g.ds).expect("the digit records");
        assert_eq!(glyphs.len(), 10);
        for (d, g8) in glyphs.iter().enumerate() {
            assert!(g8.iter().flatten().all(|&v| v == 0 || v == 2), "digit {d} is not 0/2");
        }
        for a in 0..10usize {
            for b in a + 1..10usize {
                assert_ne!(glyphs[a], glyphs[b], "digits {a} and {b} draw the same glyph");
            }
        }
    }

    /// **The frame's surround is the captured plate, minus the widgets** — the one oracle check on
    /// the whole composite.
    ///
    /// `assets/round_bg.bin` is `decode(TRAP.PIC)` plus the 802 pixels of the four widgets, so
    /// outside the widget rectangles — and outside the two viewports, which the engine fills with
    /// the room — the frame must equal it pixel for pixel. The oracle is this checkout's private
    /// capture, not something the engine reads (stage 29 deleted every loader of it); a checkout
    /// without it skips.
    #[test]
    fn the_frames_surround_is_the_captured_plate_outside_the_widgets() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(want) = crate::orig::oracle("round_bg.bin") else {
            crate::orig::skip("assets/round_bg.bin (the surround oracle)");
            return;
        };
        let g = plain();
        let frame = g.frame(false);
        let (vx0, vx1) = (round::VIEW_TOP.0, round::VIEW_TOP.0 + 176);
        let mut compared = 0usize;
        for y in 0..crate::H {
            for x in 0..crate::W {
                let in_top = x >= vx0 && x < vx1 && y >= 10 && y < 10 + round::STRIP_ROWS;
                let in_bottom = x >= vx0 && x < vx1 && y >= 118 && y < 118 + round::STRIP_ROWS;
                if in_top || in_bottom {
                    continue;
                }
                if chrome::WIDGET_RECTS
                    .iter()
                    .any(|&(x0, y0, x1, y1)| x >= x0 && x < x1 && y >= y0 && y < y1)
                {
                    continue;
                }
                compared += 1;
                assert_eq!(
                    frame.idx[y * crate::W + x],
                    want[y * crate::W + x],
                    "surround pixel ({x},{y}) is not the captured plate"
                );
            }
        }
        assert!(compared > 30_000, "the surround is most of the screen: {compared}");
    }

    /// **measured** — the trapulator dots the frame carries are the *records'* own pixels.
    ///
    /// `FUN_26c0_3278` draws seven opaque 4-row records a console; the 2x2 core the ROI reader
    /// scores is each record's own rows 1..2 at +3 px, and its value there is the record's, not this
    /// port's (`0x1584`/`0x1574`/`0x158E`/`0x157A` held, `0x15A8`/`0x1598`/`0x15B2`/`0x159E`
    /// absent). If the slot order, the bit mapping or a record were wrong, this would fail. It also
    /// pins island 1's kit from the other side: 0x083A, so two dots dark (the detonator and the
    /// water bucket) and five lit — the state the captured plate also holds.
    #[test]
    fn the_trapulator_dots_are_the_records_own_pixels() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // two consoles: a one-player round leaves the second one's tray as `TRAP.PIC`'s own bytes,
        // which is a fixed picture and not this round's kit
        let g = game(RoundOpts { players: 2, ..RoundOpts::default() });
        let f = g.frame(false);
        for (p, row) in [(0usize, items::TRAY_DOT_Y[0]), (1, items::TRAY_DOT_Y[1])] {
            for (slot, x0) in items::TRAY_DOT_X.iter().enumerate() {
                let bit = items::slot_bit(&g.exe[DS_FILE_OFF..], (slot * 2) as u16);
                let want = if g.spies[p].kit & bit != 0 { items::DOT_HELD } else { items::DOT_ABSENT };
                assert_eq!(want, if slot == 0 || slot == 2 { items::DOT_ABSENT } else { items::DOT_HELD },
                           "island 1's kit is 0x083A: bits 0 and 2 are the two dark dots");
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (x, y) = (x0 + dx, row + dy);
                        assert_eq!(f.idx[y * crate::W + x], want,
                                   "dot pixel ({x},{y}) of player {}", p + 1);
                    }
                }
            }
        }
    }

    /// The level table is **10 rows** of 12 (120 words — the island record's own count): rows 5..9
    /// are the igloo interiors, and a 5-row table cannot compose screen 14 at all.
    #[test]
    fn level_table_has_ten_rows() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = IslandData::from_exe(&exe(), 0).unwrap();
        assert_eq!(d.levels.len(), 10);
        assert_eq!(d.levels[2], [8, 8, 6, 0, 0, 2, 16, 4, 8, 8, 8, 8]);
        assert_eq!(d.levels[5], [20; 12], "the interior rows are level 20");
        assert!(d.levels[6..].iter().all(|row| row.iter().any(|&v| v == 20)),
                "rows 6..9 are interior variants");
        // and a room at screen 14 reads row 7
        let g = game(RoundOpts { start_screen: Some(14), ..RoundOpts::default() });
        assert_eq!(g.level_at(14, 0), Some(d.levels[7][0]));
    }

    // ---- stage 8b: the six owner-reported defects -------------------------------------------

    /// **Defect 1** — a press that starts and ends between two 100 ms game frames must still count.
    /// The original's `DS:0x328F`/`0x3293` are latched flags, so the press is served on the next
    /// frame; before this the port sampled the key level once per frame and a quick tap vanished.
    #[test]
    fn a_press_that_fits_inside_one_game_frame_still_moves_the_cursor() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.spies[0].trap_open = true;
        g.spies[0].trap_cursor = 4;
        // one tick down, five ticks up: the press never spans a frame boundary as a *level*
        g.tick(Input { p1: RIGHT, p2: Keys::default() });
        for _ in 1..FRAME_TICKS {
            g.tick(Input { p1: NOKEYS, p2: Keys::default() });
        }
        assert_eq!(g.spies[0].trap_cursor, 6, "the 1-tick press moved the cursor once");
        // and it moves exactly once: ten more frames with nothing held leave it where it is
        fire_frames(&mut g, NOKEYS, 10);
        assert_eq!(g.spies[0].trap_cursor, 6, "no repeat from a press that is over");
    }

    /// The same latch, for the fire key: two taps of one tick each still open the panel. With the
    /// level-only sampling the second tap was routinely lost, which is the owner's "sometimes I have
    /// to press it twice".
    #[test]
    fn two_one_tick_taps_open_the_trapulator() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { start_screen: Some(4), start_x: Some(60), start_y: Some(44),
                                     ..RoundOpts::default() });
        assert!(g.cell_of(0).map(|a| g.ds[a] == 0 && g.ds[a + 1] == 0).unwrap_or(false),
                "nothing to pick up, so a tap is the trapulator's");
        for _ in 0..2 {
            g.tick(Input { p1: FIRE, p2: Keys::default() });
            for _ in 1..FRAME_TICKS {
                g.tick(Input { p1: NOKEYS, p2: Keys::default() });
            }
            fire_frames(&mut g, NOKEYS, 1); // one clear frame between the taps
        }
        assert!(g.spies[0].trap_open, "two 1-tick taps open the panel");
    }

    /// **Defect 2** — the selection mark is a **patch of the slot's own dot cell**, not a glyph above
    /// it: 4 rows tall, 2 bytes (8 px) wide for the six tools and 1 byte (4 px) for the pickaxe and
    /// the dynamite, at x = 4*`DS:0x2E0E[cursor]` on the console's dot row. The sprite header is
    /// (rows, byte width) — stage 8 read it as (width, rows) and transposed every mark.
    #[test]
    fn the_selection_mark_is_a_patch_of_the_slots_own_cell() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let img = &exe[DS_FILE_OFF..];
        let cols = [25usize, 31, 36, 42, 47, 53, 58];
        let dot_x = items::TRAY_DOT_X;
        for (slot, &col) in cols.iter().enumerate() {
            let cursor = (slot * 2) as u16;
            for player in 0..2usize {
                let m = items::cursor_mark(img, cursor, player, false).expect("a mark");
                assert_eq!((m.x, m.y), (4 * col, 74 + 108 * player),
                           "slot {slot} player {player}: the mark sits on the console's dot row");
                assert_eq!(m.h, 4, "slot {slot}: four rows, the height of the dot cell");
                assert_eq!(m.w, if slot == 3 || slot == 5 { 1 } else { 2 },
                           "slot {slot}: the cell is 4 or 8 pixels wide");
                // the patch covers the dot: the core pixel pair sits 3 px into it
                assert!(dot_x[slot] - m.x < m.w * 4, "slot {slot}: the patch covers the dot core");
                // the two phases are the same cell with the core lit / dark
                let lit = items::cursor_mark(img, cursor, player, false).expect("lit");
                let dark = items::cursor_mark(img, cursor, player, true).expect("dark");
                let (cx, cy) = (dot_x[slot] - m.x, 1); // first core row inside the patch
                assert_eq!(lit.pixel(cx, cy), Some(1), "slot {slot}: the lit phase has cyan in the core");
                assert_eq!(dark.pixel(cx, cy), Some(0), "slot {slot}: the dark phase has black there");
            }
        }
        // and the executable's own bytes for slot 0, both phases (16 px -> 4 rows of 2 bytes)
        let lit = items::cursor_mark(img, 0, 0, false).unwrap();
        assert_eq!(lit.data, vec![0xa8, 0x2a, 0xa1, 0x4a, 0xa1, 0x4a, 0xa8, 0x2a]);
        let dark = items::cursor_mark(img, 0, 0, true).unwrap();
        assert_eq!(dark.data, vec![0xa8, 0x2a, 0xa0, 0x0a, 0xa0, 0x0a, 0xa8, 0x2a]);
    }

    /// **Defect 3** — a visitor who leaves a shared room through a north door keeps a view of his own
    /// position (`CS:0887`: `map = x - 0x16`) and the spy he left behind is not touched at all.
    ///
    /// Before the fix the port ran the *other* branch (`CS:0876`) for both cases: the leaver took the
    /// host's scroll, so his new room was composed around a column he was not standing at and he
    /// could be off the 44-unit window entirely — "his screen lights up showing the wrong scene and
    /// the spy is not visible".
    #[test]
    fn a_visitor_who_leaves_through_a_door_keeps_his_own_view() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts {
            players: 2,
            start_screen: Some(4),
            start_x: Some(120),
            start_x2: Some(141),
            start_y: Some(44),
            ..RoundOpts::default()
        });
        // the host's scroll and the visitor's, exactly as the panel state leaves them: the visitor
        // stands 38 units into the host's 44-unit window, which is what makes him a visitor
        g.spies[0].map_x = 103;
        g.spies[1].x3 = 141 * SUB;
        g.spies[1].map_x = 121;
        g.spies[1].blank = true;
        let host_map = g.spies[0].map_x;
        let door_x = g.spies[1].x();
        assert!(door_x - host_map < VISITOR_ENTRY_RIGHT, "he is the visitor when he leaves");
        let mut frames = 0;
        while g.spies[1].screen == 4 && frames < 40 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input { p1: Keys::default(), p2: UP });
            }
            frames += 1;
        }
        assert_eq!(g.spies[1].screen, 6, "the visitor went up through the door");
        assert!(!g.spies[1].blank, "his own console is live again");
        assert_eq!(g.spies[1].map_x, g.spies[1].x() - VISITOR_OWN_MAP_OFFSET,
                   "his scroll is his own position's (CS:0887), not the host's");
        assert_ne!(g.spies[1].map_x, host_map,
                   "and it is not the host's scroll, which is what put him at the wrong column");
        let on_screen = g.spies[1].x() - g.spies[1].map_x;
        assert!((0..STRIP_PITCH as i32).contains(&on_screen),
                "he is inside his own 44-unit window: x-map = {on_screen}");
        assert_eq!(g.spies[0].screen, 4, "the host keeps his room");
        assert_eq!(g.spies[0].map_x, host_map, "and his scroll");
    }

    /// The other half of defect 3: the **host** leaves and the visitor stays. `CS:0876` then gives the
    /// stayer the leaver's scroll and clears his flag, and he must still be on the old screen, in
    /// view.
    #[test]
    fn the_stayer_keeps_his_room_when_the_host_leaves() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts {
            players: 2,
            start_screen: Some(4),
            start_x: Some(141),
            start_x2: Some(125),
            start_y: Some(44),
            ..RoundOpts::default()
        });
        g.spies[0].map_x = 121;
        g.spies[1].map_x = 121;
        g.spies[1].blank = true;
        let leaver_map = g.spies[0].map_x;
        until(&mut g, UP, 40 * FRAME_TICKS as usize, |g| g.spies[0].screen != 4);
        assert_eq!(g.spies[0].screen, 6, "the host left north");
        assert_eq!(g.spies[1].screen, 4, "the stayer is still in the room he was in");
        assert!(!g.spies[1].blank, "and his console is live again");
        assert_eq!(g.spies[1].map_x, leaver_map, "with the leaver's scroll (CS:0876)");
        let on_screen = g.spies[1].x() - g.spies[1].map_x;
        assert!((0..STRIP_PITCH as i32).contains(&on_screen),
                "the stayer is inside his own window: x-map = {on_screen}");
    }

    /// **Defect 4** — two spies on one screen, both walking the same way. Each has his own scroll
    /// word, and a **visitor** moves the host's (`CS:0122`), but only inside the window the assembly
    /// tests: `CS:00cf..00df` rejects the scroll when `map + 0x2C - 0x0A <= x`, i.e. when the visitor
    /// is already past `map + 0x22`. Stage 8 had that test inverted, so a visitor standing *past* the
    /// host's right edge dragged the host's view while the host's own frame scrolled it too — the
    /// one shared `map_x` gained 4 units in a frame instead of 2, which is the owner's "twice normal
    /// speed, like it is jumping steps" and "fighting to move at the same and double speed".
    #[test]
    fn a_visitor_cannot_scroll_the_host_view_past_its_window() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts {
            players: 2,
            start_screen: Some(4),
            start_x: Some(120),
            start_x2: Some(148),
            start_y: Some(44),
            ..RoundOpts::default()
        });
        // the host stands still (so his *own* frame cannot scroll) but far enough along that the
        // assembly's second guard (`other.x - 8 >= other.map`) is satisfied
        g.spies[0].map_x = 111;
        g.spies[1].map_x = 111;
        g.spies[1].blank = true; // the visitor is drawn in the host's console
        let visit = g.spies[1].x() - g.spies[0].map_x;
        assert!((SCROLL_RIGHT_EDGE + 4..VISITOR_STEP_RIGHT).contains(&visit),
                "the visitor is past the 0x22 guard but still inside the 0x2C window: {visit}");
        assert!(g.spies[0].x() - 8 >= g.spies[0].map_x, "and the host is past the 0x08 guard");
        let host_map = g.spies[0].map_x;
        let host_x = g.spies[0].x();
        for _ in 0..8 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input { p1: NOKEYS, p2: RIGHT });
            }
        }
        assert_eq!(g.spies[0].map_x, host_map,
                   "a visitor past 0x22 must not scroll the host's view");
        assert_eq!(g.spies[0].x(), host_x, "and the host did not move either");
        assert!(g.spies[1].x() > host_x, "the visitor did walk: {}", g.spies[1].x());
    }


    /// **Defect 5** — the colour of the thing in the spy's hand, straight out of the executable's
    /// own pose-set records. For each of the four carryable parts this renders the spy twice — once
    /// with the part, once with an empty hand and the walking pose set (`0x24`) — and counts the
    /// palette indices of the pixels that differ, which is exactly the part plus the arm that holds
    /// it. Measured on the stock executable, standing pose, both spies:
    ///
    /// | code | pose set | black | cyan | magenta | white |
    /// |------|----------|-------|------|---------|-------|
    /// | 9 gyroscope | 0x60 | 20 / 4 | 0 | **14 / 11** | 2 / 3 |
    /// | 10 canister | 0x66 | 20 / 11 | 0 | 2 / 4 | 3 / 12 |
    /// | 11 punch card | 0x5A | 11 / 1 | 0 | 1 / 1 | **13 / 15** |
    /// | 12 briefcase | 0x6C | 4 / 4 | 0 | **27 / 27** | 9 / 3 |
    ///
    /// (white spy / black spy). So the punch card is drawn **white** by the original and the
    /// gyroscope **magenta** — the two are not swapped, and the card is not drawn magenta. This is
    /// the test the owner's "blue package" report needed: whatever the kit box's cyan icon suggests,
    /// the in-hand sprite of code 11 has one magenta pixel and thirteen white ones.
    #[test]
    fn the_carried_parts_are_drawn_with_the_executables_own_colours() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let pose = |code: u16| items::code_pose(&exe[DS_FILE_OFF..], code);
        assert_eq!((pose(9), pose(10), pose(11), pose(12)), (0x60, 0x66, 0x5a, 0x6c));
        let mut counts = std::collections::HashMap::new();
        for who in 0..2usize {
            for code in [9u16, 10, 11, 12] {
                let opts = RoundOpts { players: 2, start_screen: Some(4), start_x: Some(60),
                                       start_x2: Some(180), start_y: Some(44), ..RoundOpts::default() };
                let mut g = game(opts.clone());
                let mut base = game(opts);
                g.spies[who].carried = code;
                g.spies[who].set = pose(code);
                base.spies[who].carried = CARRY_NONE;
                base.spies[who].set = 0x24;
                let st = g.round_state();
                let bt = base.round_state();
                let mut a = [0u8; STRIP_LEN];
                let mut b = [0u8; STRIP_LEN];
                round::draw_spy(&mut a, &g.exe, &st.ds[..], &st, who);
                round::draw_spy(&mut b, &base.exe, &bt.ds[..], &bt, who);
                let mut c = [0usize; 4];
                for i in 0..STRIP_LEN {
                    for k in 0..4 {
                        let pa = (a[i] >> (6 - 2 * k)) & 3;
                        let pb = (b[i] >> (6 - 2 * k)) & 3;
                        if pa != pb {
                            c[pa as usize] += 1;
                        }
                    }
                }
                assert!(c.iter().sum::<usize>() > 10, "code {code} p{}: the part is drawn", who + 1);
                assert_eq!(c[1], 0, "no part uses the cyan index at all (code {code}, p{})", who + 1);
                counts.insert((who, code), c);
            }
        }
        for who in 0..2usize {
            let g9 = counts[&(who, 9)];
            let g11 = counts[&(who, 11)];
            let g12 = counts[&(who, 12)];
            assert!(g9[2] >= 10, "p{}: the gyroscope is the magenta one ({:?})", who + 1, g9);
            assert!(g11[3] >= 10 && g11[2] <= 2,
                    "p{}: the punch card is white, not magenta ({:?})", who + 1, g11);
            assert!(g12[2] >= 20, "p{}: the briefcase is magenta ({:?})", who + 1, g12);
        }
    }

    // ---- stage 9b: the two spies' bodies (CS:35e5) ---------------------------------------

    /// A round with both spies in one room, `dx0` apart on one row, no terrain in the way.
    /// Two spies standing on open ground in one room, no object layer — the fixture for the
    /// item-word transitions (the parts are written into the cells by the test itself).
    fn two_spy_room() -> Game {
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(120),
                                     start_x2: Some(140), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        g.mask = MASK_BASE;
        g.part_owner = [items::OWNER_NONE; items::PART_OWNERS];
        g
    }

    fn contact_round(dx0: i32, y2: i32) -> Game {
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(120),
                                     start_x2: Some(120 + dx0), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        if y2 != 44 {
            g.spies[1].y3 = y2 * SUB;
        }
        g
    }

    /// Drive both spies for `frames` game frames with the given key pair.
    fn both(g: &mut Game, a: Keys, b: Keys, frames: usize) {
        for _ in 0..frames * FRAME_TICKS as usize {
            g.tick(Input { p1: a, p2: b });
        }
    }

    /// **Owner defect 8**: *the spies walk straight through each other*. They do not.
    ///
    /// `CS:097c` calls `FUN_26c0_35e5` between the terrain switch and the hatch touch, and that
    /// routine is a blocking collision: `|Δy| <= 1` and `|Δx| <= 3` against the tentative position
    /// sends the mover through the same `FUN_26c0_3396` a wall does. With a 1-unit x step the
    /// closest the two can stand on one row is therefore 4 — and the recordings hold exactly that
    /// (5 dumps at dx 4, none below, `stage9b_probe.py`).
    #[test]
    fn the_two_spies_bodies_block_each_other() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = contact_round(18, 44);
        both(&mut g, RIGHT, LEFT, 60);
        let (x1, x2) = (g.spies[0].x(), g.spies[1].x());
        assert_eq!(x2 - x1, 4, "stopped 4 apart, not passed through ({x1}, {x2})");
        assert!(g.spies[0].blocked && g.spies[1].blocked, "both steps are refused");
        // and they stay there: a held key does not creep
        both(&mut g, RIGHT, LEFT, 30);
        assert_eq!((g.spies[0].x(), g.spies[1].x()), (x1, x2));

        // the same walk with the fix off is the stage-8 behaviour — they swap sides
        let mut h = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(120),
                                     start_x2: Some(138), start_y: Some(44), spy_block: false,
                                     ..RoundOpts::default() });
        h.levels = [[0u16; 12]; 10];
        both(&mut h, RIGHT, LEFT, 60);
        assert!(h.spies[0].x() > h.spies[1].x(), "--spy-block off walks through: {}, {}",
                h.spies[0].x(), h.spies[1].x());
    }

    /// The vertical half of the test is `DS:0x2F1C` = 1 against a `y` step of 2: **one row apart,
    /// the spies do not block each other** and can share an x. The corpus has the same shape — 3
    /// recorded dumps with dx 0 at dy 2 — so this is the original's rule, not a hole.
    #[test]
    fn a_spy_passes_the_other_on_the_next_row() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = contact_round(18, 46);
        both(&mut g, RIGHT, LEFT, 60);
        assert!(g.spies[0].x() > g.spies[1].x(),
                "one row down, they pass: {} vs {}", g.spies[0].x(), g.spies[1].x());
        assert!(!g.spies[0].blocked && !g.spies[1].blocked);
    }

    /// The test is **per board** in two ways. `CS:35f3` refuses a pair in different rooms, which is
    /// the obvious one; `CS:360c` additionally requires one of `0x2F7E`/`0x2F80` (a visitor) and is
    /// *unobservable* in this model, because two spies within 3 units on one screen are inside each
    /// other's 45-unit window and so at least one flag is set by construction. What this test can
    /// pin is the screen guard plus that invariant.
    #[test]
    fn the_contact_test_is_per_board() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // same coordinates, different rooms: nothing to bump into
        let mut g = contact_round(6, 44);
        g.spies[1].screen = 6;
        both(&mut g, RIGHT, LEFT, 20);
        assert!(g.spies[0].x() > g.spies[1].x(), "different rooms: they pass: {} vs {}",
                g.spies[0].x(), g.spies[1].x());
        assert!(!g.spies[0].blocked);

        // on one board, the visitor flag that `CS:360c` wants is set for at least one of them
        let mut h = contact_round(6, 44);
        both(&mut h, RIGHT, LEFT, 20);
        assert!(h.spies[0].blank || h.spies[1].blank,
                "the guard CS:360c is satisfied whenever the two are this close");
        assert!(h.spies[0].blocked || h.spies[1].blocked);
    }

    /// The meeting is **only** a refusal: nothing is taken, stowed, dropped or re-posed, and no
    /// state word moves. That is the routine's whole body (`CS:35e5..366d` writes `0x2FA4` and
    /// calls `3396`), and it is what makes the owner's "is it a fight?" answer *no*.
    #[test]
    fn walking_into_the_other_spy_takes_nothing_from_either_hand() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = contact_round(8, 44);
        g.spies[0].carried = 9;
        g.spies[0].set = 0x60;
        g.spies[1].kit = 0x6;
        g.spies[0].act_state = 0;
        g.spies[1].act_state = 0;
        both(&mut g, RIGHT, LEFT, 40);
        assert!(g.spies[0].blocked, "the step really was refused");
        assert_eq!(g.spies[0].carried, 9, "the carried part stays in hand");
        assert_eq!(g.spies[0].set, 0x60, "and so does its pose");
        assert_eq!(g.spies[1].kit, 0x6);
        assert_eq!((g.spies[0].act_state, g.spies[1].act_state), (0, 0),
                   "no reaction state is written to either spy");
        assert_eq!(g.mask, MASK_BASE, "and nothing was deposited");
        assert_eq!(g.spies[0].frame, STAND_FRAME, "the mover stands on the standing frame");
    }

    /// **The owner's defect 1.** The hatch trigger must not look at the visitor flag: the
    /// executable's `FUN_26c0_2392` (`CS:23cf..0x2443`) tests the feet cell and the other spy's
    /// animation state, and nothing else — `CS:0be7`'s `if (0x2f7e(si))` is the *object layer's*
    /// early-out, i.e. drawing, not the trigger. With the port's extra `blank` gate the spy who
    /// walks into the other's window (the visitor) never opened the hatch, which is the owner's
    /// "the black spy is not triggering the polar bear, he just walks over the hatch".
    ///
    /// The scenario is the ordinary one: the white spy stands in the room (he is the host — his
    /// console is the shared one), the black spy walks east into his window (so the port flags him
    /// the visitor by `CS:018d..0x01b4`'s window rule) and continues onto the hatch.
    #[test]
    fn the_black_spy_opens_the_hatch_even_as_the_visitor() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        let h = *g.layout.hatch.as_ref().unwrap();
        let hatch = g.hatch.addr.expect("hatch");
        let hx = 2 * h.entry as i32 + 2;
        // white: the host, standing mid-room, his scroll covering x 126..171
        g.spies[0].screen = h.band;
        g.spies[0].x3 = 150 * SUB;
        g.spies[0].y3 = 44 * SUB;
        g.spies[0].map_x = 130;
        // black: one row below, walking east into that window and onto the hatch
        g.spies[1].screen = h.band;
        g.spies[1].x3 = (hx - 12) * SUB;
        g.spies[1].y3 = 0x36 * SUB;
        g.spies[1].map_x = hx - 12 - 20;

        let mut frames = 0;
        while g.spies[1].x() < hx && frames < 120 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input { p1: NOKEYS, p2: RIGHT });
            }
            frames += 1;
        }
        assert!(g.spies[1].blank, "the walk put him inside the host's window: he is the visitor");
        assert_eq!(g.hatch.seq, HatchSeq::Descend, "the black spy opens the hatch");
        assert_eq!(g.hatch.spy, 1, "and it is HIS animation");
        assert_eq!(g.ds[hatch], round::HATCH_OPEN);
        assert!(g.spies[1].pose_raised);
        assert_eq!(g.spies[1].pose_state, round::STATE_DESCEND);
        assert_eq!((g.spies[1].x(), g.spies[1].y()), (hx, 0x36), "`CS:2436`'s landing square");
        assert_eq!(g.spies[1].dir, 4, "the descent faces into the screen");

        // 16 descent frames, then the bear's state
        for _ in 0..16 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.seq, HatchSeq::Bear);
        assert_eq!(g.spies[1].pose_state, round::STATE_BEAR);
        assert_eq!(g.spies[1].dir, 0, "the throw faces east");
        // ten frames into 0x20 the bear is written two entries right of the hatch's middle
        for _ in 0..10 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.bear_frames, 10);
        assert!(g.hatch.bear, "the bear is out");
        assert_eq!(g.ds[hatch + 2], round::BEAR);

        // the throw ends: black climbs out, WHITE laughs with the 0xDC pose set
        for _ in 0..7 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.seq, HatchSeq::Close);
        assert_eq!(g.spies[1].pose_state, round::STATE_CLOSE);
        assert_eq!(g.hatch.other, Some(0));
        assert!(g.spies[0].pose_raised, "the white spy is in the laugh");
        assert_eq!(g.spies[0].pose_state, 0x0C);
        assert_eq!(g.spies[0].set, round::anim_set(&g.ds_img(), 0x0C));
        assert_eq!(g.spies[0].set, 0xDC, "the laugh's own pose set");
        // the throw's arc, off the recorded script: +7 east and two north of the hatch
        assert_eq!((g.spies[1].x(), g.spies[1].y()), (hx + 7, 52),
                   "the recorded landing square (playtest4 ds_01783: 116,54 -> 123,52)");

        // and the close restores everything
        for _ in 0..10 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        }
        assert_eq!(g.hatch.seq, HatchSeq::Idle);
        assert_eq!(g.ds[hatch], round::HATCH_CLOSED);
        assert_eq!(g.ds[hatch + 2], 0, "the bear's byte is restored");
        assert!(!g.hatch.bear);
        // the laugh runs ten frames (`word[0x2CE6+0x0C] = 18`) and the climb-out nine, so it ends
        // one frame after the hatch: `playtest4` ds_01784, a second later, has him at set 0x24
        // with `0x2F18` clear.
        assert!(g.spies[0].pose_raised, "one frame longer than the close");
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        assert!(!g.spies[0].pose_raised, "the laugh ends");
        assert_eq!(g.spies[0].set, 0x24);
        assert!(!g.spies[1].pose_raised);
    }

    /// The same sequence, seeded from the recording's own dump: `playtest4/ds_01774` is the
    /// **black** spy entering the hatch (player 2: raised, `0x32CF` = `0x1E`, x 116, y 54) while
    /// the white spy stands at 171,44. `seed_from_dump` must put the sequence on *him* — a
    /// byte-vs-word mixup in the pose words would hand it to the white spy, which is exactly the
    /// class of bug stage 8 found in the trapulator's slot table.
    #[test]
    fn the_recorded_hatch_entry_is_the_black_spys_and_replays() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let path = crate::orig::require("recon/round/sessions/playtest4/ds_01774.bin");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipping: no recording at {}", path.display());
            return;
        };
        let st = round::RoundState::from_bytes(bytes).expect("64 KB dump");
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, ..RoundOpts::default() });
        g.seed_from_dump(&st, true);
        let hatch = g.hatch.addr.expect("the dump carries 0x31B6");
        assert_eq!(g.spies[1].pose_raised, true, "the dump's player 2 is the raised one");
        assert_eq!(g.spies[0].pose_raised, false);
        assert_eq!(g.hatch.seq, HatchSeq::Descend, "the sequence resumes on the BLACK spy");
        assert_eq!(g.hatch.spy, 1);
        let (x0, y0) = (g.spies[1].x(), g.spies[1].y());
        assert_eq!((x0, y0), (116, 54), "the recording's descent square");
        // let it run out: descend -> bear (the byte two right) -> close -> idle, and the white spy
        // laughs in between, which is what ds_01783 shows (`0x32CF` p1 = 0x0C, set 0xDC)
        let mut saw_laugh = false;
        for _ in 0..50 {
            hold(&mut g, NOKEYS, FRAME_TICKS as usize);
            if g.spies[0].pose_raised && g.spies[0].pose_state == 0x0C {
                saw_laugh = true;
            }
        }
        assert!(saw_laugh, "the white spy laughs while black is thrown");
        assert_eq!(g.hatch.seq, HatchSeq::Idle);
        assert_eq!(g.ds[hatch], round::HATCH_CLOSED);
        assert_eq!((g.spies[1].x(), g.spies[1].y()), (x0 + 7, y0 - 2),
                   "the throw lands seven east and two north, as ds_01783 records");
    }

    /// **The owner's defect 2.** `CS:3896..0x38be`: taking the briefcase stamps `DS:0x322D` —
    /// the per-spy part-owner ledger the kit box draws from — with the *holder's* `SI` for every
    /// part whose **deposited** bit is set. The port did that on a deposit and on a ground pickup
    /// but not on the case pickup, so a part another spy had found stayed lit in *his* box while
    /// the case holder's box showed it too: "both spies can show they have the same parts at once,
    /// even when one has all 3 parts in the briefcase and holds the flashing briefcase".
    #[test]
    fn taking_the_briefcase_moves_every_part_that_is_in_it_to_me() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_spy_room();
        // white found 11 and 9 (and deposited them); black found the fuel canister (10) and
        // dropped it again, which is the solid "found in the snow" state in his box.
        g.part_owner = [items::OWNER_NONE, 0, 2, 0, items::OWNER_NONE];
        g.mask = MASK_BASE | 0x40 | 0x80 | 0x200;
        g.spies[0].kit |= 0x40 | 0x200;
        g.spies[1].kit &= !0x80u16;
        assert!(g.kit_cells(1)[1], "black's box shows the fuel canister before the case moves");
        assert_eq!(g.kit_cells(0), [false, false, false],
                   "he is not holding the case, so his box shows nothing yet");
        // the case is lying on the ground under white's feet
        let feet = g.cell_of(0).expect("feet cell");
        g.ds[feet] = 12;
        assert!(g.pickup(0), "white takes the briefcase");
        assert_eq!(g.spies[0].carried, 12);
        assert_eq!(g.part_owner[2], 0, "`CS:38b1`: the fuel canister is WHITE's now");
        assert_eq!(g.part_owner[1], 0);
        assert_eq!(g.part_owner[3], 0);
        assert_eq!(g.spies[0].kit & 0x2C0, 0x2C0, "`CS:3899`: his kit takes the deposited bits");
        assert!(!g.kit_cells(1)[1], "the fuel canister is gone from the BLACK spy's box");
        assert!(g.kit_cells(0)[1], "and it is in white's, in the case");
        // putting the case down clears the part mirrors, so white's three read as FOUND (solid)
        assert!(g.drop_item(0, false));
        assert_eq!(g.spies[0].kit & !items::KIT_TOOLS, 0, "`CS:3959`: kit &= 0x83F");
        assert_eq!(g.kit_cells(0), [true, true, true]);
        assert_eq!(g.kit_cells(1), [false, false, false], "and still none of them black's");
    }

    /// How many pixels of one console's viewport the hatch's own three cells contribute: compose
    /// the strip with the hatch inked and again with the three bytes zeroed, and diff.
    fn hatch_ink(g: &mut Game, band: u16, entry: usize) -> usize {
        let addr = g.hatch.addr.expect("hatch");
        let saved = [g.ds[addr - 1], g.ds[addr], g.ds[addr + 1]];
        // the console's spy stands one row above the hatch, so he cannot cover it
        g.spies[0].screen = band;
        let hatch_x = 2 * entry as i32 + 2;
        g.spies[0].x3 = (hatch_x - 22) * SUB;
        g.spies[0].y3 = 44 * SUB;
        // the hatch's middle entry sits at screen x 22 of this console
        g.spies[0].map_x = (hatch_x - 22).clamp(0, MAP_X_MAX);
        let with = g.strip_for(0, false);
        g.ds[addr - 1] = 0;
        g.ds[addr] = 0;
        g.ds[addr + 1] = 0;
        let without = g.strip_for(0, false);
        g.ds[addr - 1] = saved[0];
        g.ds[addr] = saved[1];
        g.ds[addr + 1] = saved[2];
        with.iter().zip(without.iter()).filter(|(a, b)| a != b).count()
    }

    /// **The owner's item 4, part one.** On a **SHOW** round the hatch is level content from the
    /// placement on (`CS:269d` writes `15 0F 15`), so it is in the block and on the screen from
    /// frame 0 on every island — before anything has been picked up. The recording agrees:
    /// every sampled dump with `0x31D5 == 2` holds the triple (stage 9a, 7,570 of 7,578).
    #[test]
    fn a_show_round_draws_the_hatch_on_the_first_frame_of_every_island() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for island in 0..4usize {
            let mut g = game(RoundOpts { island, players: 2, launch: true,
                                         ..RoundOpts::default() });
            let h = g.layout.hatch.expect("a SHOW round places a hatch");
            let hatch = g.hatch.addr.expect("hatch address");
            assert_eq!(g.hatch.armed, 2, "`0x31D5` = 2 from the placement, island {island}");
            assert_eq!(g.ds[hatch], round::HATCH_CLOSED, "island {island}: 0x0F in the block");
            assert_eq!(g.ds[hatch - 1], round::HATCH_FLANK);
            assert_eq!(g.ds[hatch + 1], round::HATCH_FLANK);
            let ink = hatch_ink(&mut g, h.band, h.entry);
            assert!(ink > 0, "island {island}: the hatch is DRAWN on frame 0 ({ink} px)");
        }
    }

    /// **The owner's item 4, part two.** Nothing a spy can do to an item removes the hatch: a
    /// ground pickup, a drop, a deposit and a briefcase pickup all leave the three cells and the
    /// drawn pixels exactly as they were (the only writes to the block are the pickup's own feet
    /// cell and the hatch's own state machine).
    #[test]
    fn picking_up_and_depositing_a_part_does_not_touch_the_hatch() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true,
                                     ..RoundOpts::default() });
        let h = g.layout.hatch.expect("hatch");
        let hatch = g.hatch.addr.expect("addr");
        let before = hatch_ink(&mut g, h.band, h.entry);
        let cells = [g.ds[hatch - 1], g.ds[hatch], g.ds[hatch + 1]];

        // a part on the ground under spy 0, picked up
        let feet = g.cell_of(0).expect("feet");
        g.ds[feet] = 11;
        assert!(g.pickup(0), "the pickup ran");
        assert_eq!([g.ds[hatch - 1], g.ds[hatch], g.ds[hatch + 1]], cells,
                   "a pickup leaves the hatch alone");
        assert_eq!(hatch_ink(&mut g, h.band, h.entry), before);
        // ... and dropped again
        assert!(g.drop_item(0, false));
        assert_eq!(hatch_ink(&mut g, h.band, h.entry), before);
        // ... and deposited into the briefcase
        let feet = g.cell_of(0).expect("feet");
        g.ds[feet] = 12;
        assert!(g.pickup(0), "case");
        let feet = g.cell_of(0).expect("feet");
        g.ds[feet] = 10;
        assert!(g.pickup(0), "deposit");
        assert_eq!(g.mask & 0x80, 0x80, "the mask gained the canister's bit");
        assert_eq!([g.ds[hatch - 1], g.ds[hatch], g.ds[hatch + 1]], cells,
                   "a deposit leaves the hatch alone too");
        assert_eq!(hatch_ink(&mut g, h.band, h.entry), before);
        assert_eq!(g.hatch.armed, 2, "and the armed word is still 2");
    }

    /// **The owner's item 4, part three.** On **HIDE** the hatch does not exist until the
    /// deposited mask reaches `0x6C0` (`CS:0617`) — and then it appears, once, on that frame.
    #[test]
    fn a_hide_round_reveals_the_hatch_only_when_the_case_is_full() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: false,
                                     ..RoundOpts::default() });
        let h = g.layout.hatch.expect("the placement still stores the pointer");
        let hatch = g.hatch.addr.expect("addr");
        assert_eq!(g.hatch.armed, 0);
        assert_eq!([g.ds[hatch - 1], g.ds[hatch], g.ds[hatch + 1]], [0, 0, 0],
                   "HIDE writes no hatch bytes");
        assert_eq!(hatch_ink(&mut g, h.band, h.entry), 0, "and nothing is drawn");
        // one part short of the reveal
        g.mask = round::MASK_HATCH_REVEAL & !0x200;
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        assert_eq!(g.hatch.armed, 0, "two parts is not enough");
        assert_eq!(hatch_ink(&mut g, h.band, h.entry), 0);
        // the third part lands in the case: the reveal inks the triple on that frame
        g.mask = round::MASK_HATCH_REVEAL;
        hold(&mut g, NOKEYS, FRAME_TICKS as usize);
        assert_eq!(g.hatch.armed, 2, "`CS:0633`");
        assert_eq!([g.ds[hatch - 1], g.ds[hatch], g.ds[hatch + 1]],
                   [round::HATCH_FLANK, round::HATCH_CLOSED, round::HATCH_FLANK]);
        assert!(hatch_ink(&mut g, h.band, h.entry) > 0, "and now it is drawn");
    }

    /// Stage 17b — the computer's **position record** (`DS:0x3219` and friends) follows the
    /// items: a part taken is `0xFFFF`, a part put down is recorded at its cell (`CS:39b0`), and
    /// the briefcase put down *by a swap* (`CS:3827`: holding the case, taking a tool) is recorded
    /// too — without that last write the computer walks to a case that is not there.
    #[test]
    fn the_position_record_follows_the_parts_and_the_case() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_spy_room();
        let screen = g.spies[0].screen;
        // (a) pick a part up off the ground: its record goes to 0xFFFF
        g.links[3] = cpu::Link { used: true, screen, row: 1, col: 20 };
        let feet = g.cell_of(0).expect("feet cell");
        g.ds[feet] = 11;
        assert!(g.pickup(0));
        assert!(!g.links[3].used, "`CS:387f`: the record of a part in a hand is dropped");
        assert_eq!(g.links[3].screen, 0xFFFF);
        // (b) put it down again: recorded at the cell, with its row and entry
        g.ds[feet] = 0;
        assert!(g.drop_item(0, false));
        let block = round::block_addr(&g.ds, screen).unwrap();
        let cell = g.ds.iter().enumerate().skip(block).take(624).find(|(_, &b)| b == 11).map(|(a, _)| a).unwrap();
        let lin = cell - block;
        assert_eq!(g.links[3], cpu::Link { used: true, screen, row: (lin / 0x9C) as u8, col: (lin % 0x9C) as u8 });
        // (c) holding the case, take a saw off the ground: the case is put down in its place and
        // its record follows it (`CS:3827` -> `CS:39b0` with bp = 12)
        g.links[4] = cpu::Link::default();
        g.spies[0].carried = 12;
        g.spies[0].kit |= items::KIT_CASE_BIT;
        let feet = g.cell_of(0).expect("feet cell");
        g.ds[feet] = 2;
        assert!(g.pickup(0));
        assert_eq!(g.spies[0].carried, 2);
        assert_eq!(g.ds[feet], 12, "the case is on the cell");
        assert!(g.links[4].used, "and the record says so: {:?}", g.links[4]);
    }

    /// The owner's own example, end to end: white finds the punch card, black finds the fuel
    /// canister and drops it, white takes up a briefcase that **already has the canister in it**
    /// (black put it there on an earlier trip) and finishes the job with his own two parts.
    ///
    /// The moment that used to be wrong is right after the case changes hands: white's box
    /// flashes the canister and black's box still shows it as found — "both spies can show they
    /// have the same parts at once".
    #[test]
    fn the_owner_scenario_two_spies_never_show_the_same_part() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_spy_room();
        // (a) ground pickup: white takes the punch card (11)
        let feet = g.cell_of(0).expect("feet cell");
        g.ds[feet] = 11;
        assert!(g.pickup(0), "white picks the punch card up off the ground");
        assert_eq!(g.part_owner[3], 0, "`CS:3886`: the ground pickup stamps the ledger too");
        g.ds[feet] = 0; // the rig walks him off the cell; the card is in his hand
        // (b) black takes the fuel canister (10) up and drops it again
        let feet = g.cell_of(1).expect("feet cell");
        g.ds[feet] = 10;
        assert!(g.pickup(1), "black picks the fuel canister up");
        assert_eq!(g.part_owner[2], 2);
        assert!(g.drop_item(1, false));
        assert_eq!(g.spies[1].kit & 0x80, 0, "the drop clears his kit mirror bit");
        assert!(g.kit_cells(1)[1], "so his box shows it as FOUND (solid)");
        // (c) the briefcase on the ground already holds the canister: black put it in on an
        // earlier trip, so `0x2EB8` has bit 7 and the ledger still says black
        g.mask |= 0x80;
        let feet = g.cell_of(0).expect("feet cell");
        g.ds[feet] = 12;
        assert!(g.pickup(0), "white takes the briefcase");
        assert_eq!(g.spies[0].carried, 12);
        assert_eq!(g.part_owner[2], 0, "the canister is white's from the moment he holds the case");
        assert!(!g.kit_cells(1)[1], "black's box loses it on that same frame");
        assert!(g.kit_cells(0)[1], "and white's box shows it, in the case");
        // (d) white deposits the punch card and the gyroscope; all three are in the case
        for code in [11u8, 9u8] {
            let feet = g.cell_of(0).expect("feet cell");
            g.ds[feet] = code;
            g.spies[0].carried = 12;
            assert!(g.pickup(0), "white deposits {code} into the case");
            assert_eq!(g.spies[0].carried, 12, "and keeps the case");
        }
        assert_eq!(g.mask & 0x2C0, 0x2C0, "all three are in the case");
        assert_eq!(g.part_owner, [items::OWNER_NONE, 0, 0, 0, 0],
                   "all three parts and the case's own slot are white's");
        assert_eq!(g.kit_cells(1), [false, false, false], "black's box is empty");
        assert!(g.kit_cells(0)[0] && g.kit_cells(0)[1] && g.kit_cells(0)[2],
                "white's box shows all three, flashing");
        // (e) put the case down: the mirrors clear and the three go solid
        assert!(g.drop_item(0, false));
        assert_eq!(g.kit_cells(0), [true, true, true]);
        assert_eq!(g.kit_cells(1), [false, false, false]);
    }

    // ---- stage 13b: the dynamite stick and the plunger --------------------------------------
    //
    // `FUN_26c0_2de3`'s first branch (`CS:2dfe..2e61`) is the detonator and `FUN_26c0_373b`'s
    // `CS:37ce` is the same explosion reached by *tapping fire* on the other spy's stick. Both do
    // exactly one thing to the world: clear the one object byte under the victim — and only when
    // its low five bits are 8, because a stick buried over an object keeps the object's code.

    /// A stick planted under `victim`'s feet by `owner`: `8 | tag | 0x80` (`CS:396e`).
    fn planted(g: &mut Game, victim: usize, owner: usize) -> usize {
        let img = exe();
        let tag = items::player_tag(&img[DS_FILE_OFF..], owner);
        let a = g.cell_of(victim).expect("the spy stands on an object cell");
        g.ds[a] = items::CODE_DYNAMITE as u8 | tag | items::BURIED;
        a
    }

    /// One tap of fire by one player (button A).
    fn tap(g: &mut Game, p: usize) {
        let fire = Keys { fire: true, ..NOKEYS };
        if p == 0 {
            frame2(g, fire, NOKEYS);
        } else {
            frame2(g, NOKEYS, fire);
        }
    }

    fn plunger_of(p: usize) -> u16 {
        items::slot_code(&exe()[DS_FILE_OFF..], 0, p)
    }

    /// **read** — the two states this stage adds: `0x2CC4[0x0A]` = 0xD4 (the detonator's own
    /// four-frame pose) and `0x2CC4[0x12]` = 0x1B4 with the ceiling `0x2CE6[0x12]` = 70, i.e.
    /// **36 drawn frames** (the accepted values are 0, 2, ... 70).
    #[test]
    fn the_plunger_and_blast_tables_are_the_executables() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let img = exe();
        let img = &img[DS_FILE_OFF..];
        assert_eq!(round::anim_set(img, round::STATE_PLUNGER), 0xD4);
        assert_eq!(round::state_ceiling(img, round::STATE_PLUNGER), 6, "4 frames");
        assert_eq!(round::anim_set(img, round::STATE_BLAST), 0x1B4);
        assert_eq!(round::state_ceiling(img, round::STATE_BLAST), 70, "36 frames");
        assert_eq!(plunger_of(0), 1, "white's plunger");
        assert_eq!(plunger_of(1), 2, "black's plunger");
    }

    /// **The detonator** — `CS:2dfe..2e61`. White and black, both as the firer: the opponent
    /// standing on a stick that carries *my* tag loses the byte, gets state 0x12 and puts away
    /// what he holds; the firer gets state 0x0A whether or not there was a stick.
    #[test]
    fn the_plunger_blows_up_the_opponent_standing_on_my_stick() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for actor in 0..2 {
            let victim = 1 - actor;
            let mut g = two_at(0, 4, 120, 44, 140);
            let cell = planted(&mut g, victim, actor);
            g.set_carried(actor, plunger_of(actor));
            press_button_b(&mut g, actor);
            assert_eq!(g.ds[cell], 0, "P{} must clear the stick he set", actor + 1);
            assert_eq!(g.spies[victim].act_state, round::STATE_BLAST, "0x32CB = 0x12");
            assert_eq!(g.spies[victim].anim.map(|a| a.state), Some(round::STATE_BLAST));
            assert_eq!(g.spies[actor].act_state, round::STATE_PLUNGER, "0x32CB = 0x0A");
            assert_eq!(g.spies[actor].anim.map(|a| a.state), Some(round::STATE_PLUNGER));
            // the blast runs its 36 drawn frames and then lets him walk again — **for both
            // victims**. The detonation is armed inside the firer's body (`CS:2e30`), i.e. before
            // the draw, so whichever spy is the victim his `0x2EFC` is still the `-2` seed when
            // `CS:0d44` reaches him and the draw spends it: the animation starts on frame 2, not 0,
            // and the 36 drawn frames are 36 frames of the raise (stage 24f; the port used to give
            // player 2 a 37th frame because it never modelled the draw's clamp).
            let mut n = 0;
            while g.spies[victim].anim.is_some() && n < 60 {
                frame2(&mut g, NOKEYS, NOKEYS);
                n += 1;
            }
            assert_eq!(n, 36, "P{} blast frames after the press", victim + 1);
            assert_eq!(g.spies[victim].act_state, round::STATE_BLAST,
                       "0x32CF is not cleared by the finish (`CS:2807`)");
        }
    }

    /// A plunger with nothing of mine under the opponent is a **dud**: the victim is untouched and
    /// the firer still plays state 0x0A (`CS:2e46` pops, then falls into `2e48`). Four bytes that
    /// must all miss: an empty cell, a *plain* stick (no tag), the other spy's tagged stick, and a
    /// hazard.
    #[test]
    fn a_plunger_with_no_stick_of_mine_under_the_opponent_is_a_dud() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for actor in 0..2 {
            let victim = 1 - actor;
            let other_tag = items::player_tag(&exe()[DS_FILE_OFF..], victim);
            for byte in [
                0u8,
                items::CODE_DYNAMITE as u8,
                items::CODE_DYNAMITE as u8 | other_tag | items::BURIED,
                10, // a fuel canister: a plain object, no tag and no hazard
            ] {
                let mut g = two_at(0, 4, 120, 44, 140);
                let cell = g.cell_of(victim).expect("feet");
                g.ds[cell] = byte;
                g.set_carried(actor, plunger_of(actor));
                press_button_b(&mut g, actor);
                assert_eq!(g.ds[cell], byte, "P{} byte {byte:#04x} must survive", actor + 1);
                assert_eq!(g.spies[victim].act_state, 0, "the victim is untouched");
                assert!(g.spies[victim].anim.is_none());
                assert_eq!(g.spies[actor].act_state, round::STATE_PLUNGER);
                assert_eq!(g.item_counts[10], 1, "the dud is still a plunger pull");
            }
        }
    }

    /// The detonator is not consumed by firing it: nothing in `CS:2dfe..2e61` touches `0x2F2C`.
    /// (The recordings agree — `playtest` ds_00500 still carries code 1 while the blast runs.)
    #[test]
    fn firing_the_plunger_keeps_it_in_the_hand() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for actor in 0..2 {
            let mut g = two_at(0, 4, 120, 44, 140);
            planted(&mut g, 1 - actor, actor);
            let code = plunger_of(actor);
            g.set_carried(actor, code);
            let kit = g.spies[actor].kit;
            press_button_b(&mut g, actor);
            assert_eq!(g.spies[actor].carried, code);
            assert_eq!(g.spies[actor].kit, kit, "and the kit word does not move");
        }
    }

    /// **The other way in** — `CS:37ce`, the *pickup* path: tap fire while standing on the other
    /// spy's stick (no plunger needed, empty hand) and **the tapper** is the one who is blown up,
    /// while the owner laughs (state 0x0C). `CS:37ce` tests the other spy's tag first, before my
    /// own, and masks the byte with `0x1F`.
    #[test]
    fn tapping_fire_on_the_other_spys_stick_blows_the_tapper_up() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for owner in 0..2 {
            let victim = 1 - owner;
            let mut g = two_at(0, 4, 120, 44, 140);
            let cell = planted(&mut g, victim, owner);
            tap(&mut g, victim);
            assert_eq!(g.ds[cell], 0, "the stick goes off");
            assert_eq!(g.spies[victim].act_state, round::STATE_BLAST);
            assert_eq!(g.spies[victim].anim.map(|a| a.state), Some(round::STATE_BLAST));
            assert_eq!(g.spies[owner].act_state, round::STATE_LAUGH, "the owner laughs");
            assert_eq!(g.spies[owner].anim.map(|a| a.state), Some(round::STATE_LAUGH));
            assert_eq!(g.spies[victim].carried, items::CARRY_NONE, "no stick in hand");
        }
    }

    /// **A stick set on an object survives the blast.** `CS:3969` ORs the tag onto the byte that is
    /// already there, so the low five bits are the *object's* code; `CS:2e1a` clears the byte only
    /// when `(byte & 0x1F) == 8`. The victim still explodes — and the trap can be fired again.
    /// Measured: `m3-heat-90s` ds_00013 plants `0xCA` (black's stick on the fuel canister) and it
    /// is never cleared.
    #[test]
    fn a_stick_buried_on_an_object_explodes_but_is_not_consumed() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for actor in 0..2 {
            let victim = 1 - actor;
            let tag = items::player_tag(&exe()[DS_FILE_OFF..], actor);
            let mut g = two_at(0, 4, 120, 44, 140);
            let cell = g.cell_of(victim).expect("feet");
            let byte = 10u8 | tag | items::BURIED;
            g.ds[cell] = byte;
            g.set_carried(actor, plunger_of(actor));
            press_button_b(&mut g, actor);
            assert_eq!(g.ds[cell], byte, "a stick over a part is not cleared");
            assert_eq!(g.spies[victim].act_state, round::STATE_BLAST);
            // and again: the trap is still live
            g.spies[victim].anim = None;
            g.spies[victim].pose_raised = false;
            g.spies[actor].anim = None;
            g.spies[actor].pose_raised = false;
            frame2(&mut g, NOKEYS, NOKEYS);
            press_button_b(&mut g, actor);
            assert_eq!(g.spies[victim].act_state, round::STATE_BLAST, "still a live trap");
            assert_eq!(g.item_counts[9], 0, "no byte was cleared, so no blast was counted");
            assert_eq!(g.item_counts[10], 2, "two pulls");
        }
    }

    /// The blast puts away what the victim holds — `CS:2e2d` calls `FUN_26c0_315f` with the
    /// *victim's* `SI`. Island 1's kit already holds the saw, so the saw is **dropped**; and it is
    /// dropped into the cell the stick was in, because the byte is cleared first. A bucket (whose
    /// bit island 1's kit lacks) is **stowed** instead.
    #[test]
    fn the_blast_puts_away_what_the_victim_holds() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for actor in 0..2 {
            let victim = 1 - actor;
            // the saw: kit bit already set -> on the ground
            let mut g = two_at(0, 4, 120, 44, 140);
            let cell = planted(&mut g, victim, actor);
            g.set_carried(victim, items::CODE_SAW);
            let kit = g.spies[victim].kit;
            assert_ne!(kit & items::code_bit(&exe()[DS_FILE_OFF..], items::CODE_SAW), 0);
            g.set_carried(actor, plunger_of(actor));
            press_button_b(&mut g, actor);
            assert_eq!(g.spies[victim].carried, items::CARRY_NONE, "his hand is empty");
            assert_eq!(g.ds[cell], items::CODE_SAW as u8, "the saw lands where the stick was");
            assert_eq!(g.spies[victim].kit, kit, "a drop leaves the kit word alone");
            // the victim's own bucket (4 white / 5 black, `0x2DB6+si`): its bit is not in island
            // 1's kit, so `315f` stows it into HIS trapulator instead of dropping it
            let own_bucket = items::slot_code(&exe()[DS_FILE_OFF..], 4, victim);
            let mut g = two_at(0, 4, 120, 44, 140);
            let cell = planted(&mut g, victim, actor);
            g.set_carried(victim, own_bucket);
            let kit = g.spies[victim].kit;
            g.set_carried(actor, plunger_of(actor));
            press_button_b(&mut g, actor);
            assert_eq!(g.spies[victim].carried, items::CARRY_NONE);
            assert_eq!(g.ds[cell], 0, "nothing on the ground");
            assert_ne!(g.spies[victim].kit & 0x4, 0, "the bucket went into the trapulator");
            assert_eq!(g.spies[victim].kit, kit | 0x4);
            // …and the *other* spy's bucket is dropped instead: `315f`'s canon test
            // (`table[code] == code`) has no slot for it on this side
            let foreign = items::slot_code(&exe()[DS_FILE_OFF..], 4, actor);
            let mut g = two_at(0, 4, 120, 44, 140);
            let cell = planted(&mut g, victim, actor);
            g.set_carried(victim, foreign);
            g.set_carried(actor, plunger_of(actor));
            press_button_b(&mut g, actor);
            assert_eq!(g.spies[victim].carried, items::CARRY_NONE);
            assert_eq!(g.ds[cell], foreign as u8, "the foreign bucket is dropped");
        }
    }

    /// `CS:37aa`: my *own* stick comes back into the hand as plain dynamite — and over an object
    /// only the tag is stripped, so the object stays. (The port's first reading cleared the cell
    /// unconditionally, which eats the part underneath.)
    #[test]
    fn my_own_stick_comes_back_as_dynamite_and_leaves_the_object_under_it() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let tag = items::player_tag(&exe()[DS_FILE_OFF..], 0);
        let mut g = two_at(0, 4, 120, 44, 140);
        let cell = g.cell_of(0).expect("feet");
        g.ds[cell] = 10 | tag | items::BURIED; // a stick over the fuel canister
        tap(&mut g, 0);
        assert_eq!(g.spies[0].carried, items::CODE_DYNAMITE);
        assert_eq!(g.ds[cell], 10, "`& 0x1F` strips the tag; the part is still there");
        assert_ne!(g.spies[0].kit & items::code_bit(&exe()[DS_FILE_OFF..], 8), 0);
        // the plain case: 8 | tag | 0x80 -> the cell goes
        let mut g = two_at(0, 4, 120, 44, 140);
        let cell = planted(&mut g, 0, 0);
        tap(&mut g, 0);
        assert_eq!(g.spies[0].carried, items::CODE_DYNAMITE);
        assert_eq!(g.ds[cell], 0);
    }

    /// Walking onto a stick — his own or the other's — is inert: `FUN_26c0_2444` returns for every
    /// `byte & 0x1F <= 0xC`, and a stick is code 8. Only a *plunger pull* or a *fire tap* sets one
    /// off; there is no pressure trigger.
    #[test]
    fn walking_onto_a_stick_does_not_set_it_off() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for owner in 0..2 {
            let walker = 1 - owner;
            let mut g = two_at(0, 4, 120, 44, 140);
            let cell = planted(&mut g, walker, owner);
            frame2(&mut g, NOKEYS, NOKEYS);
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.ds[cell], items::CODE_DYNAMITE as u8
                       | items::player_tag(&exe()[DS_FILE_OFF..], owner) | items::BURIED);
            assert_eq!(g.spies[walker].act_state, 0);
            assert!(g.spies[walker].anim.is_none());
        }
    }

    /// The bury branch's two shapes, on the world: a stick on empty ice is `0xA8`/`0xC8`, and a
    /// stick on top of a part is the part's code ORed with the planter's tag and bit 7 — and only a
    /// **stick** may land on an occupied cell (`CS:392b`'s `BX == 1` and `BP == 8` tests).
    #[test]
    fn a_planted_stick_takes_the_planters_tag_and_a_second_stick_steps_aside() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let img = exe();
        for p in 0..2 {
            let tag = items::player_tag(&img[DS_FILE_OFF..], p);
            let mut g = two_at(0, 4, 120, 44, 140);
            g.set_carried(p, items::CODE_DYNAMITE);
            let cell = g.cell_of(p).expect("feet");
            g.ds[cell] = 0;
            press_button_b(&mut g, p);
            assert_eq!(g.ds[cell], 8 | tag | items::BURIED, "P{} on empty ice", p + 1);
            assert_eq!(g.spies[p].carried, items::CARRY_NONE);
            // over a part: the part's code survives in the low five bits
            let mut g = two_at(0, 4, 120, 44, 140);
            g.set_carried(p, items::CODE_DYNAMITE);
            let cell = g.cell_of(p).expect("feet");
            g.ds[cell] = 10;
            press_button_b(&mut g, p);
            assert_eq!(g.ds[cell], 10 | tag | items::BURIED, "P{} over a part", p + 1);
            // a stick already there: `(byte & 0x60) != 0` sends the new one west/east
            let mut g = two_at(0, 4, 120, 44, 140);
            g.set_carried(p, items::CODE_DYNAMITE);
            let cell = g.cell_of(p).expect("feet");
            let other = items::player_tag(&img[DS_FILE_OFF..], 1 - p);
            g.ds[cell] = 8 | other | items::BURIED;
            g.ds[cell - 1] = 0;
            g.ds[cell + 1] = 1; // west first, so this does not matter — but it must not be used
            press_button_b(&mut g, p);
            assert_eq!(g.ds[cell], 8 | other | items::BURIED, "the existing stick is untouched");
            assert_eq!(g.ds[cell - 1], 8 | tag | items::BURIED, "the new one steps west");
        }
    }

    /// **Drawing** — nothing new is needed for a stick, and this pins why: the object layer's
    /// `CMP AL,0x14; JGE` is a *signed* compare, so every byte with bit 7 takes the shared record
    /// `DS:0x6F80` whatever its low bits are (`round::draw_objects`). A planted stick is therefore
    /// a **mound** on the ice — which is the whole point of burying it — and only a stick dropped
    /// in the open (`0x08`) draws the dynamite sprite `DS:0x2D38[8] = 0x6D74`.
    #[test]
    fn a_planted_stick_is_drawn_as_a_mound_like_any_buried_byte() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let render = |byte: u8| {
            let mut g = two_at(0, 4, 120, 44, 140);
            let cell = items::cell_ahead(&g.ds, 4, g.spies[0].x(), g.spies[0].y(), g.spies[0].mirror)
                .expect("the cell in front");
            g.ds[cell] = byte;
            (cell, g.strip_for(0, false))
        };
        let (cell, plain) = render(8);              // a stick in the open
        let (_, buried_item) = render(0x88);        // a buried stick (a mound)
        let (_, white) = render(0xA8);              // white's planted stick
        let (_, black) = render(0xC8);              // black's planted stick
        let (_, on_part) = render(0xA4);            // a stick set on a buried bucket
        assert_eq!(white, buried_item, "0xA8 draws exactly like 0x88 — the shared bit-7 record");
        assert_eq!(black, buried_item, "and so does 0xC8");
        assert_eq!(on_part, buried_item, "and 0xA4: the low five bits are not drawn");
        let diff = plain.iter().zip(white.iter()).filter(|(a, b)| a != b).count();
        assert!(diff > 10, "an open stick (0x08 at {cell:#06x}) is a different sprite: {diff} bytes");
    }

    // ---- stage 13b: the recorded corpus ------------------------------------------------------

    /// **23 recorded plants, every one at a spy's own feet and carrying his tag.** `FUN_26c0_38e1`
    /// writes `carried | tag | 0x80` and lands the byte at the feet (or one entry west, or east,
    /// when the feet cell is taken), so each plant byte must carry a tag bit and bit 7, and a spy
    /// whose feet cell is within one entry of it must be the one whose tag it carries.
    #[test]
    fn every_recorded_stick_plant_is_a_tagged_buried_byte_at_the_planters_feet() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["audio-smoke", "m1-fix-960", "m3-heat", "m3-heat-90s", "playtest",
                        "playtest2", "playtest5", "rocket-room-1", "rocket-room-2",
                        "rocket-room-4-hard", "trapulator-island4"];
        let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let mut plants = 0usize;
        let mut at_birth = 0usize;
        for ses in sessions {
            let dumps = dumps_of(ses);
            if dumps.len() < 2 {
                continue;
            }
            for w in dumps.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                let tags = [u16::from_le_bytes([a[items::DS_PLAYER_TAG], a[items::DS_PLAYER_TAG + 1]])
                                as u8,
                            u16::from_le_bytes([a[items::DS_PLAYER_TAG + 2],
                                                a[items::DS_PLAYER_TAG + 3]]) as u8];
                for band in [0u16, 2, 4, 6, 8] {
                    let Some(base) = round::block_addr(&a, band) else { continue };
                    // band 8's row 0 is the live 24-byte structure at 0x5E00, not object data
                    let lo = if band == 8 { round::OBJ_ROW_LEN } else { 0 };
                    for off in lo..0x270usize {
                        let byte = b[base + off];
                        if byte & items::OWNER_TAGS == 0 || a[base + off] & items::OWNER_TAGS != 0 {
                            continue; // a stick that was not there in the dump before
                        }
                        plants += 1;
                        assert_ne!(byte & items::BURIED, 0,
                                   "{ses}/{}: a planted stick is buried ({byte:#04x})",
                                   w[1].file_name().unwrap().to_string_lossy());
                        let code = byte & items::CODE_MASK;
                        assert!(code <= items::CODE_LAST_ITEM as u8,
                                "{ses}: {byte:#04x} has an item code");
                        assert_ne!(byte & items::OWNER_TAGS, items::OWNER_TAGS,
                                   "{ses}: {byte:#04x} carries both tags");
                        // the planter: a spy on that band whose feet cell is within one entry
                        let mut who = None;
                        for i in 0..2usize {
                            let so = 2 * i;
                            let band_i = w16(&b, 0x329F + so);
                            if band_i != band {
                                continue;
                            }
                            let Some(feet) = items::cell_addr(&b, band_i,
                                w16(&b, 0x329B + so) as i32, w16(&b, 0x3297 + so) as i32)
                            else { continue };
                            let foot = feet - base as usize;
                            if foot.abs_diff(off) <= 1 && byte & tags[i] != 0 {
                                who = Some((i, foot as i32 - off as i32));
                            }
                        }
                        let (i, delta) = who.unwrap_or_else(|| panic!(
                            "{ses}/{}: nobody at {byte:#04x} band{band} off {off:#x}",
                            w[1].file_name().unwrap().to_string_lossy()));
                        at_birth += 1;
                        assert!(delta >= -1 && delta <= 1, "the cell steps at most one entry");
                        // and the reverse: a spy whose tag it carries must be that spy
                        assert_ne!(byte & tags[i], 0);
                    }
                }
            }
        }
        eprintln!("stage 13b: {plants} recorded stick plants, {at_birth} at the planter's own cell");
        assert_eq!(plants, 23, "the corpus's stick plants");
        assert_eq!(at_birth, plants);
    }

    /// **The five recorded sticks that leave their cell under a standing spy, replayed byte for
    /// byte** — three explosions (`playtest` ds_00500, `rocket-room-2` ds_00566,
    /// `rocket-room-4-hard` ds_01136: white pulls the plunger, black is on the 0xA8) and two
    /// pickups (`playtest2` ds_00117, `trapulator-island4` ds_00194: white takes his own stick back
    /// as dynamite). The gesture is chosen from the *before* dump's own words, exactly as a player
    /// would have made it.
    #[test]
    fn every_recorded_stick_that_leaves_its_cell_replays_byte_for_byte() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["playtest", "playtest2", "rocket-room-2", "rocket-room-4-hard",
                        "trapulator-island4", "m3-heat-90s", "playtest5"];
        let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let img = exe();
        let (mut blasts, mut picks) = (0usize, 0usize);
        for ses in sessions {
            let dumps = dumps_of(ses);
            for w in dumps.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for v in 0..2usize {
                    let so = 2 * v;
                    let band = w16(&a, 0x329F + so);
                    let Some(cell) = items::cell_addr(&a, band, w16(&a, 0x329B + so) as i32,
                                                      w16(&a, 0x3297 + so) as i32)
                    else { continue };
                    let before = a[cell];
                    if before & items::OWNER_TAGS == 0 || b[cell] == before {
                        continue;
                    }
                    if w16(&a, 0x2F70 + so) != 0 || w16(&a, 0x2F64 + so) >= 2 {
                        continue;
                    }
                    let o = 1 - v;
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    for k in 0..2 {
                        g.spies[k].anim = None;
                        g.spies[k].pose_raised = false;
                    }
                    g.hatch.other = None;
                    let name = format!("{ses}/{}", w[1].file_name().unwrap().to_string_lossy());
                    let blast = b[0x32CF + so] == round::STATE_BLAST as u8 && b[0x2F18 + so] != 0;
                    let mut bad = Vec::new();
                    if blast {
                        let code = items::slot_code(&img[DS_FILE_OFF..], 0, o);
                        assert_eq!(w16(&a, 0x2F2C + 2 * o), code,
                                   "{name}: the firer's hand holds his plunger");
                        assert!(!g.spies[o].trap_open, "{name}: the panel is not open");
                        press_button_b(&mut g, o);
                        blasts += 1;
                        if g.spies[v].anim.map(|x| x.state) != Some(round::STATE_BLAST) {
                            bad.push(format!("victim state {:?}", g.spies[v].anim));
                        }
                        if g.spies[o].act_state != round::STATE_PLUNGER {
                            bad.push(format!("firer state {:#04x}", g.spies[o].act_state));
                        }
                    } else {
                        let carried_after = w16(&b, items::DS_COST_CARRIED + 2 * v);
                        if carried_after != items::CODE_DYNAMITE {
                            continue; // not a stick event the code has a path for
                        }
                        tap(&mut g, v);
                        picks += 1;
                        if g.spies[v].carried != items::CODE_DYNAMITE {
                            bad.push(format!("carried {}", g.spies[v].carried));
                        }
                        if g.spies[v].kit != w16(&b, items::DS_COST_KIT + 2 * v) {
                            bad.push(format!("kit {:#06x}", g.spies[v].kit));
                        }
                    }
                    if g.ds[cell] != b[cell] {
                        bad.push(format!("cell {cell:#06x} {:#04x} not {:#04x}", g.ds[cell], b[cell]));
                    }
                    if g.spies[v].carried != w16(&b, items::DS_COST_CARRIED + 2 * v) {
                        bad.push(format!("victim carried {} not {}", g.spies[v].carried,
                                         w16(&b, items::DS_COST_CARRIED + 2 * v)));
                    }
                    if g.spies[v].kit != w16(&b, items::DS_COST_KIT + 2 * v) {
                        bad.push(format!("victim kit {:#06x} not {:#06x}", g.spies[v].kit,
                                         w16(&b, items::DS_COST_KIT + 2 * v)));
                    }
                    assert!(bad.is_empty(), "{name} P{} byte {before:#04x}: {}", v + 1,
                            bad.join("; "));
                }
            }
        }
        eprintln!("stage 13b: {blasts} recorded detonations and {picks} recorded stick pickups \
replayed byte for byte");
        assert_eq!(blasts, 3, "the corpus's plunger detonations");
        assert_eq!(picks, 2, "the corpus's stick pickups");
    }

    /// **The three recorded duds.** `playtest` ds_00496 and `cold-check-large-1-5` ds_00206/215: a
    /// spy fires his plunger with nothing of his under the other spy. The firer still plays 0x0A;
    /// no byte moves and the other spy is not in 0x12.
    #[test]
    fn every_recorded_dud_plunger_leaves_the_world_alone() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["playtest", "cold-check-large-1-5"];
        let w16 = |d: &[u8], o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
        let img = exe();
        let mut duds = 0usize;
        for ses in sessions {
            let dumps = dumps_of(ses);
            for w in dumps.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for i in 0..2usize {
                    let so = 2 * i;
                    // a pull that has just started: state 0x0A, raised, and the pose frame still
                    // at 0 — the dump catches it on the frame `CS:277c` seeded from -2. (The state
                    // word `0x32CF` is never cleared, so a stale 0x0A outlives its animation and
                    // cannot be used to spot the *start* of a run.)
                    if b[0x32CF + so] != round::STATE_PLUNGER as u8 || b[0x2F18 + so] == 0
                        || w16(&b, round::DS_POSE_FRAME + so) != 0
                    {
                        continue;
                    }
                    // …and a *dud* in the recording: the cell under the other spy did not change
                    // between the two dumps. A hit is the other test's business.
                    let o = 1 - i;
                    if b[0x329F + so] != a[0x329F + so] {
                        continue;
                    }
                    let Some(ocell) = items::cell_addr(&a, w16(&a, 0x329F + 2 * o),
                        w16(&a, 0x329B + 2 * o) as i32, w16(&a, 0x3297 + 2 * o) as i32)
                    else { continue };
                    if b[ocell] != a[ocell] {
                        continue;
                    }
                    let code = items::slot_code(&img[DS_FILE_OFF..], 0, i);
                    if w16(&a, 0x2F2C + 2 * i) != code {
                        continue;
                    }
                    let Some(cell) = items::cell_addr(&a, w16(&a, 0x329F + 2 * o),
                        w16(&a, 0x329B + 2 * o) as i32, w16(&a, 0x3297 + 2 * o) as i32)
                    else { continue };
                    let before = a[cell];
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    for k in 0..2 {
                        g.spies[k].anim = None;
                        g.spies[k].pose_raised = false;
                    }
                    g.hatch.other = None;
                    press_button_b(&mut g, i);
                    let name = format!("{ses}/{}", w[1].file_name().unwrap().to_string_lossy());
                    duds += 1;
                    assert_eq!(g.ds[cell], before, "{name}: a dud must not touch the cell");
                    assert_eq!(g.spies[i].act_state, round::STATE_PLUNGER,
                               "{name}: the firer still plays 0x0A");
                    assert_ne!(g.spies[o].anim.map(|x| x.state), Some(round::STATE_BLAST),
                               "{name}: the other spy is not blown up");
                }
            }
        }
        eprintln!("stage 13b: {duds} recorded dud plunger pulls replayed");
        assert!(duds >= 2, "expected the corpus's dud pulls, found {duds}");
    }
// ---- stage 14: body heat, the bar, sinking and the freeze --------------------------------
//
// Every constant below is the executable's own: the heat word (`DS:0x2F58`+2i), the sink word
// (`0x2F68`+2i), the two per-spy counters (`0x2F54` the base drain, `0x2F60` the terrain rate) and
// the frozen word (`0x2F64`+2i). The switch is `FUN_26c0_1e0f`, the heat step is `FUN_26c0_21bd`,
// the death is `FUN_26c0_2238` and the bar is drawn from `0x21bd`'s tail. See `STAGE14_REPORT.md`.

/// `CS:0745` — the base drain: one step of `+2` cold per `0x8A` frames of any outdoor life.
const HEAT_BASE_PERIOD: u16 = 0x8A;
/// `DS:0x2F58` starts at 3 (bar full), `0x3D` is the last survivable value and the test is `>`.
const HEAT_FULL: u16 = 3;
const HEAT_FREEZE_AT: u16 = 0x3D;

/// Put a spy on the first x with terrain code `want` on `screen`, at y = 44, and return it.
fn x_on_code(g: &Game, screen: u16, want: u16, y: i32) -> Option<i32> {
    (0..=300).find(|x| g.terrain.code(&g.levels, screen, *x, y) == Some(want))
}

/// The first `(screen, x)` whose terrain code is `want` at row `y`, searched over the round's own
/// level table — which is what matters for the igloo interiors, because `place_igloo_fires` decides
/// which of them have a fire (`0x12` -> `0x16`) at round start.
fn find_code(g: &Game, want: u16, y: i32) -> Option<(u16, i32)> {
    for row in 0..10usize {
        for col in 0..12usize {
            let screen = (row as u16) * 2;
            let base = g.terrain.gate(col);
            for off in 0..26 {
                if g.terrain.code(&g.levels, screen, base + off, y) == Some(want) {
                    return Some((screen, base + off));
                }
            }
        }
    }
    None
}

/// A game whose clock cannot run out under the test — a 30-step heat series is 4140 frames and
/// island 1's round is 360 s (3600 frames), so the short islands would expire mid-replay.
fn untimed(mut g: Game) -> Game {
    // The clock is a count of **counts** now (9 frames each), so "long enough" is a count no test
    // can reach: 65,535 counts is 97 minutes of play. (Stage 16a; this used to be a tick count.)
    g.clock_left = u16::MAX;
    g.clock_word = 0;
    g
}

/// A game with two spies standing on opposite sides of the room, both on plain snow.
fn two_on_snow(island: usize, screen: u16, y: i32) -> Game {
    let x1 = x_on_code(
        &game(RoundOpts { island, start_screen: Some(screen), ..RoundOpts::default() }),
        screen,
        terrain::T_OPEN,
        y,
    )
    .expect("open ground on this screen");
    two_at(island, screen, x1, y, x1 + 40)
}

/// **The base drain** — `CS:0745`..`075e`: `0x2F54` counts up every frame and at `0x8A` (138) calls
/// `FUN_26c0_21bd` with `AX = -1`, which adds `2` to the cold word and clears the counter. The word
/// starts at 3 (full bar) and `0x3D` (61) is the last value that is not freezing.
///
/// Measured: `freeze-death` (island 7, both spies standing still on open snow) climbs 3 -> 63 in
/// **30 steps of 2**, one every 15.0 s of the recording, i.e. one every 138 game frames.
#[test]
fn body_heat_starts_full_and_drains_two_per_hundred_and_thirty_eight_frames() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let mut g = untimed(two_on_snow(0, 4, 44));
    assert_eq!(g.spies[0].heat, HEAT_FULL, "the round starts with a full bar");
    frame2(&mut g, NOKEYS, NOKEYS);
    assert_eq!(g.spies[0].heat, HEAT_FULL, "one frame is not a step");
    assert_eq!(g.spies[0].base_t, 1, "`0x2F54` counts the frame");
    for _ in 0..(HEAT_BASE_PERIOD - 1) {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert_eq!(g.spies[0].heat, HEAT_FULL + 2, "the 138th frame is one step");
    assert_eq!(g.spies[0].base_t, 0, "and `21bd` clears the counter");
    // thirty steps takes him to 63 — the recorded freeze-death value — and no further
    for _ in 0..(29 * HEAT_BASE_PERIOD) {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert_eq!(g.spies[0].heat, 63, "3 + 30 x 2, the recorded ceiling");
    for _ in 0..(4 * HEAT_BASE_PERIOD) {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert_eq!(g.spies[0].heat, 63, "the add is refused above 0x3D (`CS:21f4`)");
}

/// **The two words are per spy** — `0x2F58` (P1) and `0x2F5A` (P2), the `+si` indexing the earlier
/// stages got wrong twice. A frozen spy's word must not move his opponent's.
#[test]
fn each_spy_has_his_own_heat_and_sink_word() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let mut g = two_on_snow(0, 4, 44);
    g.spies[1].heat = 40;
    frame2(&mut g, NOKEYS, NOKEYS);
    assert_eq!(g.spies[0].heat, 3);
    assert_eq!(g.spies[1].heat, 40);
    let st = g.round_state();
    assert_eq!(u16::from_le_bytes([st.ds[0x2F58], st.ds[0x2F59]]), g.spies[0].heat);
    assert_eq!(u16::from_le_bytes([st.ds[0x2F5A], st.ds[0x2F5B]]), g.spies[1].heat);
    assert_eq!(u16::from_le_bytes([st.ds[0x2F68], st.ds[0x2F69]]), g.spies[0].pose_clip);
}

/// **Deep snow** — `CS:1ea0`: the sink word climbs by 2 to the `6` plateau (8 is written and
/// immediately clamped back, `CS:1eac`), the shared counter `0x2F60` steps every **nine** passes
/// (`CS:1ebb` is `jg 8`, stage 22a — it was written as eight) and each step costs 2 cold.
/// `DS:0x2F2C == 7` (the snowshoes **in hand**) returns at `CS:1e92` *before* the sink or the
/// counter is touched.
#[test]
fn deep_snow_sinks_to_six_and_drains_two_per_nine_frames() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let snow = x_on_code(&game(RoundOpts { island: 1, start_screen: Some(4), ..RoundOpts::default() }),
                         4, 0x02, 44)
        .expect("island 2 has a deep-snow cell on screen 4");
    let mut g = two_at(1, 4, snow, 44, snow + 40);
    assert_eq!(g.terrain.code(&g.levels, 4, snow, 44), Some(0x02));
    let mut sinks = Vec::new();
    for _ in 0..8 {
        frame2(&mut g, NOKEYS, NOKEYS);
        sinks.push(g.spies[0].pose_clip);
    }
    assert_eq!(sinks, vec![2, 4, 6, 6, 6, 6, 6, 6], "2 a frame to the 6 plateau, never 8");
    // eight passes is not yet a step: the ninth is (`CS:1ebb` `jg 8`)
    assert_eq!(g.spies[0].heat, 3, "eight frames on deep snow is still full");
    frame2(&mut g, NOKEYS, NOKEYS);
    assert_eq!(g.spies[0].heat, 5, "the ninth frame of the branch = +2 cold");
    for _ in 0..8 {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert_eq!(g.spies[0].heat, 5, "and eight more is still not a step");
    frame2(&mut g, NOKEYS, NOKEYS);
    assert_eq!(g.spies[0].heat, 7, "the ninth is");

    // the snowshoes in hand: no sink, no counter, no drain (CS:1e92 jumps over the whole branch)
    let mut g = two_at(1, 4, snow, 44, snow + 40);
    g.set_carried(0, 7);
    for _ in 0..40 {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert_eq!(g.spies[0].pose_clip, 0, "the snowshoes keep him on top");
    assert_eq!(g.spies[0].heat, HEAT_FULL, "and cost him nothing");
    assert_eq!(g.spies[0].terrain_t, 0, "`0x2F60` was never touched");
}

/// **Water at the island edge** — `CS:1ed9`: sink 2 a frame to the `0x14` (20) plateau, and the
/// counter steps every **12** frames for 2 cold. The same branch takes `0x17`, the second water
/// code. Measured across the corpus: every code-1 sample is on the ramp or at 20 (`sink_hist`), and
/// code 1 is only ever on levels 4, 6 and 8 — the outside plates, the ocean band among them.
#[test]
fn water_sinks_to_twenty_and_drains_faster_than_snow() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let mut g = two_at(0, 4, 29, 44, 90);
    assert_eq!(g.terrain.code(&g.levels, 4, 29, 44), Some(0x01), "the level-8 ocean band");
    let mut sinks = Vec::new();
    for _ in 0..12 {
        frame2(&mut g, NOKEYS, NOKEYS);
        sinks.push(g.spies[0].pose_clip);
    }
    assert_eq!(sinks, vec![2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 20, 20],
               "2 a frame to the 20 plateau, never 22");
    // 12 frames of the branch = one step, so frame 12 above is the first
    assert_eq!(g.spies[0].heat, 5, "twelve frames in the water = +2 cold");
    for _ in 0..12 {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert_eq!(g.spies[0].heat, 7);
    // stepping back onto snow clears the sink (`CS:2019`)
    let mut back = two_at(0, 4, 29, 44, 90);
    back.spies[0].pose_clip = 20;
    back.spies[0].screen = 4;
    // walk east off the water: the dispatch on open ground zeroes the clip
    let open = x_on_code(&back, 4, terrain::T_OPEN, 44).unwrap();
    back.spies[0].x3 = open * SUB;
    frame2(&mut back, NOKEYS, NOKEYS);
    assert_eq!(back.spies[0].pose_clip, 0, "open ground clears the clip");
}

/// **The igloo interior warms** — `CS:1ffa`: codes `0x0C` and `0x1D` step the counter every
/// `0x14` (21) frames and each step takes 2 cold *off* the word (`AX = +1`). Measured on
/// `playtest4` (x 139, y 48, 204 s) and `playtest5`: -44 .. -51 cold per minute, i.e. one step
/// every 2.3 s — the 21 frames at the original's own frame length.
#[test]
fn the_igloo_interior_warms_two_per_twenty_one_frames() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let mut g = untimed(game(RoundOpts { island: 0, start_screen: Some(18), ..RoundOpts::default() }));
    let (screen, x) = find_code(&g, 0x1D, 44).expect("an igloo interior floor");
    assert!(screen >= 10, "the 0x1D floor only exists indoors, screen {screen}");
    g.spies[0].screen = screen;
    g.spies[0].x3 = x * SUB;
    g.spies[0].y3 = 44 * SUB;
    g.spies[0].heat = 30;
    for _ in 0..20 {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert_eq!(g.spies[0].heat, 30, "twenty frames is not yet a step (`CS:2003` is `jle`)");
    frame2(&mut g, NOKEYS, NOKEYS);
    assert_eq!(g.spies[0].heat, 28, "the 21st frame is: one step of warming");
    // and it refuses below 4 (`CS:21d6`)
    g.spies[0].heat = 3;
    for _ in 0..42 {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert_eq!(g.spies[0].heat, 3, "the bar cannot fill past full");
}

/// **The fire** — code 6 is the lit fireplace (levels 16 and 22 only; the interior mask carries a
/// 4x3 block of them). `CS:1f0d`: `3396` (the spy is stopped, the step refused) and a step every
/// **2** frames, warming 2 each time — ten times the interior floor's rate, which is why the owner
/// remembers the fire.
#[test]
fn pressing_into_the_fire_warms_two_per_two_frames_and_refuses_the_step() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let mut g = game(RoundOpts { island: 0, start_screen: Some(18), ..RoundOpts::default() });
    let (screen, fire) = find_code(&g, 0x06, 38).expect("an igloo's lit fire (place_igloo_fires)");
    g.spies[0].screen = screen;
    g.spies[0].x3 = (fire - 1) * SUB;
    g.spies[0].y3 = 38 * SUB;
    g.spies[0].heat = 40;
    let before = g.spies[0].x();
    for _ in 0..4 {
        frame2(&mut g, RIGHT, NOKEYS);
    }
    assert_eq!(g.spies[0].x(), before, "the fire is solid: 3396 refuses the step");
    assert_eq!(g.spies[0].heat, 36, "four frames against the fire = two steps of warming");
}

/// **The ice** — `CS:1e2d`: only a *moving* spy rolls (`0x2F00 + 0x2F04 != 0`), the roll is the
/// PRNG's low byte and it slips when that is **above 0xE6** (25 of 256, 9.8 %), costing 6 cold and
/// the ordinary fall state `0x10` — no byte is written or cleared, because ice is terrain, not a
/// trap. The roll is taken once per frame of movement, so it is *not* reproducible from a dump;
/// what is pinned here is the threshold and the consequence.
#[test]
fn the_ice_slip_costs_six_cold_and_puts_the_spy_in_the_fall() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let probe = game(RoundOpts { island: 1, start_screen: Some(6), ..RoundOpts::default() });
    // the longest run of ice on the row, so he can stay on it while bouncing
    let mut best = (0i32, 0i32);
    let mut run = (0i32, 0i32);
    for x in 0..=300 {
        if probe.terrain.code(&probe.levels, 6, x, 44) == Some(0x03) {
            if run.1 - run.0 == 0 {
                run.0 = x;
            }
            run.1 = x + 1;
            if run.1 - run.0 > best.1 - best.0 {
                best = run;
            }
        } else {
            run = (0, 0);
        }
    }
    let ice = (best.0 + best.1) / 2;
    assert!(best.1 - best.0 >= 4, "island 2's screen 6 has an ice run ({best:?})");
    let mut g = two_at(1, 6, ice, 44, ice + 40);
    assert_eq!(g.terrain.code(&g.levels, 6, ice, 44), Some(0x03));
    g.spies[0].heat = 20;
    g.set_carried(0, 3); // the saw: a tool, so the put-away stows it
    // bounce on the ice: the roll only happens on a frame the spy is *moving* (`CS:1e35`), and
    // 25 of 256 is one slip per ten frames on average
    let dirs = [RIGHT, LEFT];
    let mut slipped = 0;
    for n in 0..400 {
        frame2(&mut g, dirs[n % 2], NOKEYS);
        if g.spies[0].anim.map(|a| a.state) == Some(round::STATE_FALL) {
            slipped += 1;
            assert_eq!(g.spies[0].act_state, round::STATE_FALL);
            assert_eq!(g.spies[0].carried, CARRY_NONE, "`315f` puts the saw away");
            assert_eq!(g.spies[1].act_state, round::STATE_LAUGH, "the other spy laughs");
            break;
        }
    }
    assert!(slipped > 0, "the 10 % roll never fired in 400 moving frames");
    assert!(g.spies[0].heat >= 20 + 6, "the slip costs 3 cold steps");
}

/// **The freeze** — `CS:0745` and `FUN_26c0_2238`. Above `0x3D` (61) the spy is stopped, whatever
/// he holds is put away, the pose set becomes `0x72` for good, the *other* spy laughs and he plays
/// state `0x14` (pose set `0x228`, ceiling 110 -> **56 frames**) with `0x2F64 = 1`. When that
/// animation ends `0x2F64` becomes **2** — out of the round — and the room passes to the survivor
/// (`CS:294d` into the igloo-exit bookkeeping at `CS:2953`).
///
/// Measured, `freeze-death` ds_00031 -> 00037: heat 61 -> 63, the other spy's `0x32CB` = `0x0C`,
/// then `0x2F64` 1 with `0x32CF` = `0x14` and `0x2EF8` = `0x228`, then `0x2F64` = 2 with `0x2EF8` =
/// `0x72` — and no respawn, no life, no score: the statue stays for the 30 s the recording ran on.
#[test]
fn freezing_plays_state_fourteen_then_puts_the_spy_out_of_the_round() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let mut g = two_on_snow(0, 4, 44);
    g.spies[0].heat = HEAT_FREEZE_AT;
    frame2(&mut g, NOKEYS, NOKEYS);
    assert_eq!(g.spies[0].heat, 61, "0x3D is still survivable (`jle`)");
    g.spies[0].heat = HEAT_FREEZE_AT + 1;
    frame2(&mut g, NOKEYS, NOKEYS);
    assert_eq!(g.spies[0].frozen, 1, "`0x2F64` = 1");
    assert_eq!(g.spies[0].act_state, round::STATE_FREEZE);
    assert_eq!(g.spies[0].anim.map(|a| a.state), Some(round::STATE_FREEZE));
    assert_eq!(g.spies[0].set, 0x228, "`word[0x2CC4 + 0x14]`");
    assert_eq!(g.spies[1].act_state, round::STATE_LAUGH, "the other spy laughs");
    // 56 frames: 110/2 + 1
    for _ in 0..55 {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    assert!(g.spies[0].anim.is_some(), "still freezing on frame 56");
    frame2(&mut g, NOKEYS, NOKEYS);
    assert!(g.spies[0].anim.is_none());
    assert_eq!(g.spies[0].frozen, 2, "out of the round (`CS:294d`)");
    assert!(g.spies[0].out_of_round);
    assert_eq!(g.spies[0].set, 0x72, "and the frozen pose set is what he keeps");
    // the statue never moves again
    let (x, y) = (g.spies[0].x(), g.spies[0].y());
    for _ in 0..200 {
        frame2(&mut g, RIGHT, NOKEYS);
    }
    assert_eq!((g.spies[0].x(), g.spies[0].y()), (x, y), "`0x2F64 >= 2` skips his frame");
    assert_eq!(g.spies[0].frozen, 2);
}

/// **What the freeze does to the hands** — `CS:2251`: `FUN_26c0_315f` (a tool into its slot, a part
/// on the ground) and then, if a part is *still* in hand, `FUN_26c0_2327` finds it a free cell and
/// drops it there. A recorded freeze is on nobody's hands (`freeze-death` carried -1 throughout),
/// so the branch is pinned here as the code has it.
#[test]
fn the_freeze_empties_the_hands_and_drops_the_part_on_the_ice() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    for victim in 0..2 {
        let mut g = two_on_snow(0, 4, 44);
        g.spies[victim].heat = HEAT_FREEZE_AT + 1;
        g.set_carried(victim, 9); // a rocket part
        frame2(&mut g, NOKEYS, NOKEYS);
        assert_eq!(g.spies[victim].frozen, 1, "P{}", victim + 1);
        assert_eq!(g.spies[victim].carried, CARRY_NONE, "the hand is emptied");
        assert_eq!(g.spies[1 - victim].act_state, round::STATE_LAUGH);
    }
}

/// **The bar**: nine pixels wide at x 58..66, **59 rows**, rows 9..67 for the top console and
/// 117..175 for the bottom; one row per cold unit above 3, white from the top, cyan below
/// (`heat = 62 - value` as the ROI has it), stamped from the two 14-byte row records at
/// `DS:0x15EE`/`DS:0x15FC` by `chrome::draw_heat_gauge` (`CS:0x21BD`).
///
/// The gauge's own bitmap is checked against those records in `chrome::tests`; what this test pins
/// is that the engine's frame carries the drawn bar, at the console's coordinates, for every level
/// in range — including the two ends, where a level outside 3..62 must not spill out of the gauge.
#[test]
fn the_heat_bar_is_one_row_per_cold_unit() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    for (heat, want) in [(3u16, 0usize), (4, 1), (30, 27), (62, 59), (63, 59)] {
        let mut g = plain();
        g.spies[0].heat = heat;
        g.spies[1].heat = heat;
        let f = g.frame(false);
        for (y0, tag) in [(9usize, "top"), (117, "bottom")] {
            for r in 0..59usize {
                let v = f.idx[(y0 + r) * crate::W + 63];
                let white = r < want;
                assert_eq!(v, if white { 3 } else { 1 },
                           "heat {heat} {tag} row {r} (want {want} white rows)");
            }
        }
    }
}

/// **The whole recorded drift, replayed** — `freeze-death` is the session built for this: island 7,
/// two spies, nobody touches a key, and every dump carries `DS:0x2F58`/`0x2F5A`. Seeded from
/// ds_00002 (both spies already at their standing positions, both on code 0) the engine's own
/// counter must produce the recorded series frame for frame: **31 values, 3 -> 63**, one every
/// 138 frames, both players identical (`8.0` cold a minute, which is what the ROI measured).
#[test]
fn the_recorded_heat_series_replays_from_its_own_dump() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let Some(st) = session_dump("freeze-death", "ds_00002.bin") else { return };
    let mut g = game(RoundOpts { island: 6, players: 2, ..RoundOpts::default() });
    g.players = 2;
    g.seed_from_dump(&st, true);
    assert_eq!(g.terrain.code(&g.levels, g.spies[0].screen, g.spies[0].x(), g.spies[0].y()),
               Some(terrain::T_OPEN), "P1 stands on clear snow");
    assert_eq!(g.terrain.code(&g.levels, g.spies[1].screen, g.spies[1].x(), g.spies[1].y()),
               Some(terrain::T_OPEN), "and so does P2");
    let mut got = Vec::new();
    for _ in 0..31 {
        got.push((g.spies[0].heat, g.spies[1].heat));
        for _ in 0..HEAT_BASE_PERIOD {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
    }
    let want: Vec<u16> = (0..31).map(|k| 3 + 2 * k as u16).collect();
    let p1: Vec<u16> = got.iter().map(|x| x.0).collect();
    let p2: Vec<u16> = got.iter().map(|x| x.1).collect();
    assert_eq!(p1, want, "P1's recorded series");
    assert_eq!(p2, want, "P2's recorded series (2F5A, the +2 index)");
    // the recorded dumps agree value for value: ds_00003..ds_00031 climb 3,5,..,61, ds_00032 63
    let mut matched = 0;
    for k in 0..30usize {
        let name = format!("ds_{:05}.bin", k + 3);
        if let Some(d) = session_dump("freeze-death", &name) {
            let h = u16::from_le_bytes([d.ds[0x2F58], d.ds[0x2F59]]);
            assert_eq!(h, want[k + 1], "{name} carries {h}, the model says {}", want[k + 1]);
            matched += 1;
        }
    }
    eprintln!("stage 14: {matched} recorded heat dumps matched the replayed series");
    assert!(matched >= 25, "expected the session's dumps, found {matched}");
}

/// **The freeze cascade, replayed from the dump before it** — seeded with ds_00033 (P2 already
/// freezing, P1 still laughing off P2's death at heat 63) the engine must produce what ds_00034..37
/// record: P1 freezes on the next frame, P2 comes out of it first (`0x2F64 = 2`, pose `0x72`, the
/// visitor flag set — the room handed to P1), then P1, and both keep the frozen pose with both
/// console flags set, which is the state the recording ends in.
#[test]
fn the_recorded_freeze_cascade_replays_from_the_dump_before_it() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let Some(st) = session_dump("freeze-death", "ds_00033.bin") else { return };
    let mut g = game(RoundOpts { island: 6, players: 2, ..RoundOpts::default() });
    g.players = 2;
    g.seed_from_dump(&st, true);
    assert_eq!(g.spies[0].heat, 63);
    assert_eq!(g.spies[1].frozen, 1, "P2 was already freezing in the dump");
    frame2(&mut g, NOKEYS, NOKEYS);
    assert_eq!(g.spies[0].frozen, 1, "P1 freezes on the first frame (ds_00034)");
    assert_eq!(g.spies[0].set, 0x228);
    // P2 started earlier, so he comes out first: two ceilings of 56 frames at most
    let mut p2_out = None;
    let mut p1_out = None;
    for n in 0..140 {
        frame2(&mut g, NOKEYS, NOKEYS);
        if p2_out.is_none() && g.spies[1].frozen == 2 {
            p2_out = Some(n);
            assert_eq!(g.spies[1].set, 0x72, "the frozen pose set");
            assert!(g.spies[1].blank, "0x2F7E = 1: the room went to the other spy");
            assert_eq!(g.spies[0].frozen, 1, "P1 is still in his own freeze");
        }
        if p1_out.is_none() && g.spies[0].frozen == 2 {
            p1_out = Some(n);
            assert!(g.spies[0].blank);
            assert!(g.spies[1].blank, "and P2 keeps his flag");
        }
        if p1_out.is_some() && p2_out.is_some() {
            break;
        }
    }
    assert!(p2_out.is_some() && p1_out.is_some(), "both must come out of the freeze");
    assert!(p2_out.unwrap() < p1_out.unwrap(), "the spy who froze first comes out first");
    assert!(p1_out.unwrap() - p2_out.unwrap() < 60, "the two freezes are the same length");
    // the recording's last dumps: both out, both flags, both in 0x72, no respawn and no life lost
    for _ in 0..200 {
        frame2(&mut g, NOKEYS, NOKEYS);
    }
    for i in 0..2 {
        assert_eq!(g.spies[i].frozen, 2);
        assert_eq!(g.spies[i].set, 0x72);
        assert!(g.spies[i].blank);
        assert_eq!((g.spies[i].x(), g.spies[i].y()), (if i == 0 { 128 } else { 266 }, 44),
                   "the statue never moves and nobody respawns");
    }
}

/// **The recorded sink words** — every dump's `DS:0x2F68` against the terrain code the same dump
/// carries: on water (code 1/0x17) it is 0 or an even value up to **20**, on deep snow (code 2) 0
/// up to **6**, on ice (code 3) always 0, and on every other walkable code 0. This is the check
/// that the terrain classification is the original's and not a guess: 23k dumps of it.
#[test]
fn every_recorded_sink_word_lies_on_the_models_ramp_for_its_terrain() {
    if crate::orig::absent() {
        crate::orig::skip("the original game data");
        return;
    }
    let mut counts = std::collections::BTreeMap::<u16, usize>::new();
    let mut bad = Vec::new();
    let mut seen = std::collections::BTreeMap::<u16, Vec<u16>>::new();
    for ses in ["freeze-death", "playtest1", "playtest2", "playtest4", "playtest5",
                "trapulator-island4", "rocket-room-3-hard", "cold-check-1-5"] {
        for path in dumps_of(ses).into_iter().take(400) {
            let Ok(st) = round::RoundState::from_ds_dump(&path) else { continue };
            let mut g = game(RoundOpts { island: 0, players: 2, ..RoundOpts::default() });
            g.levels = st.levels;
            for i in 0..2usize {
                let p = &st.players[i];
                if p.blank && i == 1 {
                    continue;
                }
                let Some(c) = g.terrain.code(&g.levels, p.band, p.spy.x as i32, p.spy.y as i32)
                else { continue };
                // the hatch's script writes `0x2F68` itself (`CS:0a01`: 2,4,..,28), so a
                // descending spy is not a terrain sample
                let raised = u16::from_le_bytes([st.ds[0x2F18 + 2 * i], st.ds[0x2F19 + 2 * i]]) != 0;
                let pstate = u16::from_le_bytes([st.ds[0x32CF + 2 * i], st.ds[0x32D0 + 2 * i]]);
                if raised && [0x1C, 0x1E, 0x20].contains(&pstate) {
                    continue;
                }
                let sink = u16::from_le_bytes([st.ds[0x2F68 + 2 * i], st.ds[0x2F69 + 2 * i]]);
                counts.entry(c).and_modify(|n| *n += 1).or_insert(1);
                seen.entry(c).or_default().push(sink);
                let ok = match c {
                    0x01 | 0x17 => sink <= 20 && sink % 2 == 0,
                    0x02 => sink <= 6 && sink % 2 == 0,
                    _ => sink == 0,
                };
                if !ok {
                    bad.push((ses, path.file_name().unwrap().to_string_lossy().to_string(),
                              i, c, sink));
                }
            }
        }
    }
    for (c, v) in &seen {
        let mut h = std::collections::BTreeMap::new();
        for x in v { *h.entry(*x).or_insert(0usize) += 1; }
        eprintln!("code {c:#04x}: {h:?}");
    }
    eprintln!("stage 14: {} sink samples over {} codes, {} off the ramp: {:?}",
              counts.values().sum::<usize>(), counts.len(), bad.len(), &bad[..bad.len().min(5)]);
    assert!(counts.get(&0x01).copied().unwrap_or(0) > 20, "water samples must be present");
    assert!(counts.get(&0x02).copied().unwrap_or(0) > 20, "deep-snow samples must be present");
    assert!(bad.is_empty(), "{} dumps carry a sink the model cannot produce", bad.len());
}


    // ---- stage 15: the launch, the ascent, and every end of round --------------------------

    /// The launch, driven all the way through the descent so `launch` has actually fired.
    fn launched_game() -> Game {
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        g.spies[0].kit |= round::KIT_ALL_PARTS;
        // `0x1E` is sixteen script frames; the launch is the branch its end takes (`CS:284d`).
        // Stop **on** that frame: the ascent's first step is the launch frame's own.
        let mut guard = 0;
        while g.hatch.seq != HatchSeq::Launched {
            frame(&mut g);
            guard += 1;
            assert!(guard < 40, "the descent never fired the launch");
        }
        g
    }

    /// One game frame (the original's own cadence, `FRAME_TICKS` ticks).
    fn frame(g: &mut Game) {
        hold(g, NOKEYS, FRAME_TICKS as usize);
    }

    /// The ascent's length: `0x2ebc` runs 2, 4, .., `0xee` — 119 frames (`CS:20f2`, `CS:21ae`).
    const ASCENT_FRAMES: u16 = 119;

    /// `CS:20f2` + `CS:21ae` — the ascent is a **counter, not a cut**: `0x2ebc += 2` a frame from
    /// 2 to `0xee` (238), and at `0xee` the round-over flag `0x248a` goes up, which the outer loop
    /// (`CS:0649`) turns into "reset and back to the options screen".
    ///
    /// Measured: all 8 launch frames in the corpus read `0x2ebc` = 2, and every launch's next dump
    /// (15 s later) reads `0x2ebc` = 238 with `0x324d` = 0 — the options phase. The recorded frames
    /// of `rocket-room-2` put 387 video frames (12.90 s at 30 fps) between the launch and the
    /// options screen, against 119 game frames at the measured 108 ms = 12.85 s.
    #[test]
    fn the_ascent_runs_119_frames_and_then_ends_the_round() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = launched_game();
        assert_eq!(g.rocket_asc, 2, "the launch frame is the ascent's first step (recorded 2)");
        // and the *words* are the recording's: the launcher's own 0x2f64 stays 0 (the port wrote 2
        // there before this stage) while 0x2f70 is 1 and the other spy is out
        let st = g.round_state();
        assert_eq!(u16::from_le_bytes([st.ds[round::DS_SPY_STATE], st.ds[round::DS_SPY_STATE + 1]]),
                   0, "0x2f64(launcher) = 0, as all 20 recorded launch dumps have it");
        assert_eq!(u16::from_le_bytes([st.ds[0x2F70], st.ds[0x2F71]]), 1, "0x2f70 = 1");
        assert_eq!(u16::from_le_bytes([st.ds[round::DS_SPY_STATE + 2],
                                       st.ds[round::DS_SPY_STATE + 3]]), 2,
                   "0x2f66 = 2: the other spy is out of the round");
        assert_eq!(g.phase, Phase::Playing, "the round runs on while the rocket climbs");
        assert_eq!(g.end, None, "and nothing has ended it yet");
        for _ in 0..(ASCENT_FRAMES - 2) {
            frame(&mut g);
        }
        assert_eq!(g.rocket_asc, round::ASCENT_END - 2, "one frame short of the flag");
        assert_eq!(g.phase, Phase::Playing, "`CS:21ae` has not fired yet");
        frame(&mut g);
        assert_eq!(g.rocket_asc, round::ASCENT_END, "0xee, the value the recording ends on");
        assert_eq!(g.phase, Phase::Over);
        assert_eq!(g.end, Some(RoundEnd::Launch), "the launch's own exit, not the clock");
        // and a finished round does not move
        let x = g.spies[0].x();
        hold(&mut g, RIGHT, 60);
        assert_eq!(g.spies[0].x(), x);
    }

    /// `CS:20ee` — the rocket is the record at `DS:0x61fa` (160 rows x 9 bytes, two planes),
    /// blitted at bottom `0x36` with `0x31b2` = `0x9e - 0x2ebc`; when that goes negative the sprite
    /// stops sliding and its bottom starts rising instead (`CS:212a`).
    #[test]
    fn the_ascent_draws_the_rocket_record_rising_out_of_the_hatch() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = launched_game();
        let ds = g.round_state().ds;
        let rec = round::rocket_record(&ds).expect("DS:0x61fa is a record");
        assert_eq!((rec.height, rec.width), (160, 9), "the record's own header, as dumped");

        assert_eq!(round::ascent_place(2, round::ROCKET_SHIFT_BASE), (54, 156));
        assert_eq!(round::ascent_place(158, round::ROCKET_SHIFT_BASE), (54, 0));
        assert_eq!(round::ascent_place(200, round::ROCKET_SHIFT_BASE), (12, 0),
                   "past 0x9e the bottom rises instead of the top sinking");
        assert_eq!(54 - 160 + 156, 50, "so the launch frame shows the nose at rows 50..53");

        // **The record's own first eight rows are transparent** (`and` = 0xFF, `or` = 0x00): its art
        // starts at sprite row 8 and ends at 151. So the launch frame — which shows sprite rows
        // 0..3 at viewport rows 50..53 — draws *nothing at all*, and the tip appears on the frame
        // where row 8 reaches the room, `54 - 160 + shift + 8 >= 0`.
        assert_eq!(rec.and[..8 * 9], [0xFFu8; 72], "eight transparent rows at the top");
        assert!(rec.and[8 * 9..9 * 9].iter().any(|&b| b != 0xFF), "the art starts on row 8");
        assert!(rec.and[148 * 9..151 * 9].iter().any(|&b| b != 0xFF), "and ends at 151");

        // the drawn strip agrees, row for row. `asc = 238` draws nothing at all (past the body's
        // 0xd2 and the flame's 0xec), so it is the room's own reference.
        g.rocket_asc = round::ASCENT_END;
        let room = g.strip_for(0, false);
        g.rocket_asc = 2;
        assert_eq!(g.strip_for(0, false), room,
                   "the launch frame: rows 50..53 are the record's transparent padding");
        // the *visible* top: the window's top is `52 - asc`, the art starts at sprite row 8, and the
        // art's own row 8 lands on plain snow and does not change a byte — so the first row that
        // differs is one below the window's top plus eight. It climbs 2 px a frame, stops at the
        // room's ceiling, and past `0x9e` the bottom rises instead.
        let first_row = |g: &Game, room: &[u8; STRIP_LEN]| -> (usize, usize) {
            let strip = g.strip_for(0, false);
            let rows: Vec<usize> = (0..STRIP_ROWS)
                .filter(|&r| (0..STRIP_PITCH)
                    .any(|c| strip[r * STRIP_PITCH + c] != room[r * STRIP_PITCH + c]))
                .collect();
            (*rows.first().unwrap_or(&0), *rows.last().unwrap_or(&0))
        };
        let mut tops = Vec::new();
        let mut lasts = Vec::new();
        for asc in [20u16, 22, 40, 60, 158, 200] {
            g.rocket_asc = asc;
            let (a, b) = first_row(&g, &room);
            tops.push(a);
            lasts.push(b);
        }
        // the model's window top is `52 - asc` and the art starts at sprite row 8, so the art's own
        // first row lands at `60 - asc`; against the snow the first *differing* row can sit one or
        // two rows lower (some art rows reproduce the plate), never more.
        for (asc, top) in [20u16, 22, 40].iter().zip(&tops[..3]) {
            let want = 60i32 - *asc as i32;
            assert!((*top as i32 - want).abs() <= 2,
                    "asc {asc}: the art's first visible row {top} against the model's {want}");
        }
        assert_eq!(&tops[3..], &[0, 0, 0], "clipped at the room's ceiling");
        // the lowest ink is the hatch's own row 53 until the body's bottom starts to rise at
        // `0x9e`. At 200 the body covers rows 0..11 and the **flame** (drawn from `0x96`) reaches
        // lower, to row 31 — which is why that sample's last row is the flame's and not the body's;
        // the body's own bottom is `ascent_place(200) = (12, 0)`, asserted above.
        assert_eq!(lasts, vec![53, 53, 53, 53, 53, 31]);
        // the columns are the record's own 9 bytes, at the hatch's column
        g.rocket_asc = 158;
        let strip = g.strip_for(0, false);
        let cols: Vec<usize> = (0..STRIP_PITCH)
            .filter(|&c| (0..STRIP_ROWS).any(|r| strip[r * STRIP_PITCH + c] != room[r * STRIP_PITCH + c]))
            .collect();
        assert_eq!(cols.first().copied(), Some(20), "2*col - view + 2, the hatch's own column");
        assert!(cols.len() <= 10, "nine bytes wide at most ({} cols)", cols.len());
    }

    /// `CS:2156` — the flame is drawn from `0x2ebc` = `0x96` to `0xec`, four columns right of the
    /// body, with its shift based on `0xba`, and its record alternates every step: the pair at
    /// `DS:0x2d34`/`0x2d36` is chosen by `0x2ebc & 2`.
    #[test]
    fn the_flame_appears_at_0x96_and_alternates_its_two_records() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = launched_game();
        let ds = g.round_state().ds;
        let a = round::flame_record(&ds, 0x96).expect("a flame record");
        let b = round::flame_record(&ds, 0x98).expect("the other one");
        assert_eq!((a.height, a.width), (38, 5), "the flame's own header");
        assert_ne!(a.or, b.or, "the two records are different art, not the same one twice");
        assert_eq!(round::flame_record(&ds, 0x94).map(|r| r.height), Some(38),
                   "0x94 still uses the pair; the *gate* is 0x96 and lives in the caller");
        // the caller's gate (`CS:2148`) and range (`CS:215c`), as numbers
        assert!(round::ASCENT_FLAME <= 0x96 && round::ASCENT_FLAME_END >= 0xec);
        // and the flame's own placement uses 0xba, so it tracks the body 28 px lower
        assert_eq!(round::ascent_place(0x96, round::FLAME_SHIFT_BASE), (54, 0xba - 0x96));
    }

    /// **stage 19a, defect 2a** — the flame's column is the **body's `+2`**, not `+4`.
    ///
    /// `CS:211c` computes the body's `cx = 2*[0x320F] - [0x24A6] + 2`; `CS:217b` computes the
    /// flame's as the *same* expression with `+4`, i.e. body + 2 bytes. The body record is 9 bytes
    /// (36 px) and the flame 5 (20 px), so `+2` is what centres it. Stage 15 added the `+4` to the
    /// body's already-shifted x, which put the flame 2 bytes — 8 px — right of the recording's own
    /// flame (measured: `playtest4` ds_01814's ascent, asc 160..210, the flame's 20-px box at
    /// viewport x 176..199, not 184..207).
    #[test]
    fn the_flame_sits_two_bytes_right_of_the_body_not_four() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = launched_game();
        // `asc = 238` draws neither body nor flame: the room's own reference
        g.rocket_asc = round::ASCENT_END;
        let room = g.strip_for(0, false);
        g.rocket_asc = 180;
        let strip = g.strip_for(0, false);
        let ds = g.round_state().ds;
        let rec = round::flame_record(&ds, 180).expect("a flame record");
        let x = g.rocket_x - g.spies[0].map_x + 2;
        let (bottom, shift) = round::ascent_place(180, round::FLAME_SHIFT_BASE);
        let top = (bottom - rec.height as i32 + shift).max(0);
        // how many of the record's mask-opaque pixels sit at column `x0` with the record's own ink
        let matched = |strip: &[u8; STRIP_LEN], x0: i32| -> (usize, usize) {
            let (mut ok, mut n) = (0usize, 0usize);
            for r in 0..rec.height {
                let y = top + r as i32;
                if y < 0 || y >= STRIP_ROWS as i32 {
                    continue;
                }
                for c in 0..rec.width {
                    let a = rec.and[r * rec.width + c];
                    let o = rec.or[r * rec.width + c];
                    for k in 0..4 {
                        if (a >> (6 - 2 * k)) & 3 != 0 {
                            continue; // the mask keeps the room here
                        }
                        let xb = x0 + c as i32;
                        if xb < 0 || xb >= STRIP_PITCH as i32 {
                            continue;
                        }
                        n += 1;
                        let px = (strip[y as usize * STRIP_PITCH + xb as usize] >> (6 - 2 * k)) & 3;
                        if px == (o >> (6 - 2 * k)) & 3 {
                            ok += 1;
                        }
                    }
                }
            }
            (ok, n)
        };
        let (ok2, n) = matched(&strip, x + 2);
        assert!(n > 200, "the flame has {n} opaque pixels on screen at asc 180");
        assert_eq!((ok2, n), (n, n), "every one of them is at the body's + 2");
        let (ok4, _) = matched(&strip, x + 4);
        assert!(ok4 * 2 < n, "at the body's + 4 only {ok4} of {n} match — the old column");
        // and the flame really is drawn: the strip differs from the room at its own rows
        let changed = (0..STRIP_ROWS)
            .filter(|&r| (0..STRIP_PITCH).any(|c| strip[r * STRIP_PITCH + c] != room[r * STRIP_PITCH + c]))
            .count();
        assert!(changed > 10, "the flame's rows differ from the bare room: {changed}");
    }

    /// **stage 19a, defect 2b** — `CS:2865..2877` stages the launcher's own `[19][5]` picture into
    /// the rocket record's **OR** plane (`0x679C`), rows `0x32 - h .. 0x31`, columns 2..6 of the
    /// 9-byte row: the porthole. The shipped record fills those rows' OR plane with a flat `0xAA`
    /// (index 2, magenta) behind a `0x00` mask, so without the copy the window renders as plain pink
    /// — the owner's "just FLAT PINK in that area".
    #[test]
    fn the_launch_stages_the_spy_into_the_rockets_window() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let template = exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000].to_vec();
        for (who, staged) in [(0usize, 0x3C32usize), (1, 0x3C93)] {
            let mut g = two_at(0, 4, 120, 44, 126);
            g.launch(who);
            let ds = g.round_state().ds;
            let (h, w) = (ds[round::DS_ROCKET] as usize, ds[round::DS_ROCKET + 1] as usize);
            assert_eq!((h, w), (160, 9), "the record's own header");
            let orp = round::DS_ROCKET + 2 + h * w;
            let (sh, sw) = (ds[staged] as usize, ds[staged + 1] as usize);
            assert_eq!((sh, sw), (19, 5), "the staged picture's header, as `0x205d` reads it");
            let top = 0x32 - sh;
            for r in 0..sh {
                for c in 0..sw {
                    assert_eq!(
                        ds[orp + (top + r) * w + 2 + c],
                        ds[staged + 2 + r * sw + c],
                        "the window's row {} col {}", top + r, 2 + c
                    );
                }
            }
            // the template the port starts from holds the flat pink the defect showed
            assert_eq!(
                &template[orp + (top + 1) * w + 2..orp + (top + 1) * w + 6],
                &[0xAA; 4],
                "the shipped record's window is a flat 0xAA in the OR plane"
            );
            let rec = round::rocket_record(&ds).expect("the rocket record");
            assert_eq!(
                &rec.or[(top + 1) * w + 2..(top + 1) * w + 7],
                &ds[staged + 2 + sw..staged + 2 + 2 * sw],
                "and the record the drawing path reads carries it"
            );
            // the drawn strip: at asc 60 the window's second row lands on strip row 24, so its
            // pixels are the record's OR (the picture), not the pink the template holds
            g.rocket_asc = 60;
            let strip = g.strip_for(who, false);
            let x = g.rocket_x - g.spies[who].map_x + 2;
            let (bottom, sh2) = round::ascent_place(60, round::ROCKET_SHIFT_BASE);
            let skip = (h as i32 - bottom - sh2).max(0);
            let row = (top as i32 + 1 - skip) as usize;
            assert!(row < STRIP_ROWS, "the window's second row is on screen at asc 60");
            // (`sw - 1`: the record's last byte of that row is `0x03`, so its rightmost pixel keeps
            // the destination — the window's edge is not the picture's)
            for c in 0..sw - 1 {
                assert_eq!(
                    strip[row * STRIP_PITCH + (x + 2 + c as i32) as usize],
                    ds[staged + 2 + sw + c],
                    "the window's second row, byte {c}, is the spy's picture"
                );
            }
        }
    }

    /// The same copy, measured on the recordings: **every** launch frame in the corpus whose ascent
    /// has just started carries it — the dump's OR plane equals the template with the copy applied,
    /// byte for byte, for both spies.
    #[test]
    fn every_recorded_launch_dump_carries_the_window_picture() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["playtest4", "playtest5", "rocket-room-2", "trapulator-island4"];
        let exe = exe();
        let template = &exe[DS_FILE_OFF..DS_FILE_OFF + 0x10000];
        let mut seen = 0;
        let mut launchers = [false; 2];
        for ses in sessions {
            let dir = crate::orig::require("recon/round/sessions").join(ses);
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("ds_") || !name.ends_with(".bin") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(e.path()) else { continue };
                let Ok(st) = round::RoundState::from_bytes(bytes) else { continue };
                let word = |a: usize| u16::from_le_bytes([st.ds[a], st.ds[a + 1]]);
                let rk = [word(0x2F70), word(0x2F72)];
                if (rk[0] == 0 && rk[1] == 0) || word(0x2EBC) != 2 {
                    continue; // not a launch frame
                }
                let who = if rk[0] != 0 { 0 } else { 1 };
                let (h, w) = (st.ds[round::DS_ROCKET] as usize, st.ds[round::DS_ROCKET + 1] as usize);
                let orp = round::DS_ROCKET + 2 + h * w;
                let staged = word(0x2E06 + 2 * who) as usize;
                let (sh, sw) = (st.ds[staged] as usize, st.ds[staged + 1] as usize);
                let top = 0x32 - sh;
                for r in 0..sh {
                    for c in 0..sw {
                        assert_eq!(
                            st.ds[orp + (top + r) * w + 2 + c],
                            st.ds[staged + 2 + r * sw + c],
                            "{ses}/{name}: player {}'s window byte ({}, {})", who + 1, top + r, 2 + c
                        );
                    }
                }
                // and the record is *not* the template's pink: at least the picture's own row 0
                assert_ne!(
                    &st.ds[orp + top * w + 2..orp + top * w + 7],
                    &template[orp + top * w + 2..orp + top * w + 7],
                    "{ses}/{name}: the window is the spy, not the flat 0xAA"
                );
                seen += 1;
                launchers[who] = true;
            }
        }
        assert!(seen >= 4, "the corpus has four launch frames, saw {seen}");
        assert_eq!(launchers, [true, true], "and both spies are among them");
    }

    /// `CS:065e` — the clock reaching 0 does **not** end the round while a spy is animating
    /// (`0x2f18`/`0x2f1a`) or a rocket is flying (`0x2f70`/`0x2f72`). The original waits, drawing,
    /// until both are clear and only then leaves the round.
    #[test]
    fn the_clock_at_zero_waits_for_the_animations_to_finish() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.clock_left = 1;
        g.clock_word = 0;
        // a spy in a state that keeps `0x2f18` up (the freeze's, but any 0x1e+ state does)
        g.spies[0].pose_raised = true;
        g.spies[0].pose_state = round::STATE_FREEZE;
        g.spies[0].anim = Some(Anim { state: round::STATE_FREEZE, frame: -2, cell: None });
        for _ in 0..(CLOCK_FRAMES * FRAME_TICKS) as usize {
            g.tick(Input::default());
        }
        assert_eq!(g.clock_left, 0, "the clock is out");
        assert_eq!(g.phase, Phase::Playing, "but the animation holds the round open");
        // the animation ends: now the round is over, and the reason is the clock
        g.spies[0].pose_raised = false;
        g.spies[0].anim = None;
        frame(&mut g);
        assert_eq!(g.phase, Phase::Over);
        assert_eq!(g.end, Some(RoundEnd::Clock));
    }

    /// `CS:06c0` — **both spies out of the round** goes straight back to the outer loop, i.e. the
    /// options screen, with no timer wait. This is what the freeze-death recording shows: the
    /// second statue's animation ends at t 471.3 s and the same instant's frame is already the
    /// options panel with the clock at 00:00 (`freeze-death/f_014139.jpg`), while the *first*
    /// statue alone never ends anything.
    #[test]
    fn both_spies_out_of_the_round_end_it_at_once() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.players = 2;
        g.spies[0].frozen = 2;
        g.spies[0].out_of_round = true;
        frame(&mut g);
        assert_eq!(g.phase, Phase::Playing, "one statue is not the end of the round");
        g.spies[1].frozen = 2;
        g.spies[1].out_of_round = true;
        frame(&mut g);
        assert_eq!(g.phase, Phase::Over);
        assert_eq!(g.end, Some(RoundEnd::BothOut));
    }

    /// One spy frozen and the other launching: the launch writes `0x2f64(other)` = 2 on a spy who
    /// is already out, so the ascent is still what ends the round — 119 frames later — and not the
    /// both-out check (`0x2f64(launcher)` stays 0, which is what the corpus shows on all 20 dumps).
    #[test]
    fn one_frozen_spy_and_a_launch_still_end_with_the_ascent() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 0, players: 2, launch: true, start_hatch: true,
                                     ..RoundOpts::default() });
        g.spies[0].kit |= round::KIT_ALL_PARTS;
        // the other spy is a statue already (`0x2f64` = 2) before the hatch is touched
        g.spies[1].frozen = 2;
        g.spies[1].out_of_round = true;
        for _ in 0..20 {
            frame(&mut g);
        }
        assert_eq!(g.hatch.seq, HatchSeq::Launched);
        assert_eq!(g.phase, Phase::Playing, "the both-out check does not fire on the launcher");
        assert_eq!(g.spies[0].frozen, 0, "0x2f64(launcher) stays 0 — measured, 20 of 20 dumps");
        for _ in 0..(ASCENT_FRAMES - 1) {
            frame(&mut g);
        }
        assert_eq!(g.phase, Phase::Over);
        assert_eq!(g.end, Some(RoundEnd::Launch), "the ascent, not the statues");
    }

    /// **The launch's words, over the whole corpus.** Every dump with a rocket flying (20 dumps,
    /// 9 sessions, both spies as launcher) has the launcher *in* the round and the other spy out
    /// and a visitor — the port wrote `0x2f64(launcher)` = 2 as well before this stage.
    #[test]
    fn every_recorded_launch_leaves_the_launcher_in_the_round() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["audio-smoke", "m3-heat-90s", "playtest", "playtest1", "playtest4",
                        "playtest5", "rocket-room-2", "rocket-room-3-hard", "trapulator-island4"];
        let mut seen = 0;
        let mut launchers = [false; 2];
        for ses in sessions {
            let dir = crate::orig::require("recon/round/sessions").join(ses);
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("ds_") || !name.ends_with(".bin") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(e.path()) else { continue };
                let Ok(st) = round::RoundState::from_bytes(bytes) else { continue };
                let word = |a: usize| u16::from_le_bytes([st.ds[a], st.ds[a + 1]]);
                let rk = [word(0x2F70), word(0x2F72)];
                if rk[0] == 0 && rk[1] == 0 {
                    continue;
                }
                let who = if rk[0] != 0 { 0 } else { 1 };
                seen += 1;
                launchers[who] = true;
                assert_eq!(word(round::DS_SPY_STATE + 2 * who), 0,
                           "{ses}/{name}: the launcher's 0x2f64 is 0, not 2");
                assert_eq!(word(round::DS_SPY_STATE + 2 * (1 - who)), 2,
                           "{ses}/{name}: the other spy is out of the round");
                assert_eq!(word(round::DS_BLANK + 2 * (1 - who)), 1,
                           "{ses}/{name}: ... and a visitor on the launcher's board");
                assert!(word(0x2EBC) >= 2, "{ses}/{name}: the ascent has started");
            }
        }
        assert!(seen >= 20, "the corpus must still carry the launches ({seen} found)");
        assert!(launchers[0] && launchers[1], "both spies launch somewhere in the corpus");
    }

    /// **The rocket's column, over the whole corpus.** `CS:20ff` reads the hatch's column
    /// (`DS:0x320f`) and puts the rocket at `2*col - view + 2`; the launch itself computes the
    /// same number into `DS:0x2ebe` (`CS:28ae`). All 20 launch dumps agree with the port's own
    /// arithmetic, which is why the port carries no separate hatch-column word: at the launch the
    /// spy stands on the hatch column, so `x - 2` is `2*col`.
    #[test]
    fn the_recorded_launch_frames_put_the_rocket_on_the_hatch_column() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut checked = 0;
        for ses in ["playtest", "playtest4", "playtest5", "rocket-room-2", "rocket-room-3-hard",
                    "trapulator-island4", "m3-heat-90s", "playtest1", "audio-smoke"] {
            let dir = crate::orig::require("recon/round/sessions").join(ses);
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("ds_") || !name.ends_with(".bin") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(e.path()) else { continue };
                let Ok(st) = round::RoundState::from_bytes(bytes) else { continue };
                let word = |a: usize| u16::from_le_bytes([st.ds[a], st.ds[a + 1]]);
                let rk = [word(0x2F70), word(0x2F72)];
                if rk[0] == 0 && rk[1] == 0 {
                    continue;
                }
                let who = if rk[0] != 0 { 0 } else { 1 };
                let col = st.ds[0x320F] as u16;
                let view = word(round::DS_MAP_X + 2 * who);
                assert_eq!(word(0x2EBE), 2 * col - view + 2,
                           "{ses}/{name}: DS:0x2ebe is the hatch column's screen x");
                let spy_x = word(round::DS_SPY_X + 2 * who);
                assert_eq!(spy_x, 2 * col + 2,
                           "{ses}/{name}: the descent leaves him on the hatch column (`CS:2436`)");
                checked += 1;
            }
        }
        assert!(checked >= 20, "{checked} launch dumps checked");
    }

    /// **A recorded launch replays from its own dump.** `playtest4` ds_01814 is the launch frame
    /// (`0x2ebc` = 2, `0x2f70` = 1 on the black spy): seeded from it the port must climb the same
    /// 118 further frames and end the round, with the ascent's counter reading what the recording
    /// reads at the end (238).
    #[test]
    fn a_recorded_launch_replays_from_its_own_dump() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(st) = session_dump("playtest4", "ds_01814.bin") else { return };
        let word = |a: usize| u16::from_le_bytes([st.ds[a], st.ds[a + 1]]);
        assert_eq!(word(0x2F70), 0, "white is the one who did not launch");
        assert_eq!(word(0x2F72), 1, "black is the launcher");
        assert_eq!(word(0x2EBC), 2, "the ascent's first step");
        let mut g = game(RoundOpts { island: 0, players: 2, ..RoundOpts::default() });
        g.seed_from_dump(&st, true);
        assert_eq!(g.phase, Phase::Playing);
        let asc = g.rocket_asc;
        assert_eq!(asc, 2, "the ascent word comes back from the dump");
        let mut n = 0;
        while g.phase == Phase::Playing && n < 400 {
            frame(&mut g);
            n += 1;
        }
        assert_eq!(g.end, Some(RoundEnd::Launch));
        assert_eq!(g.rocket_asc, round::ASCENT_END, "238, the value ds_01815 carries");
        assert_eq!(n, (ASCENT_FRAMES - 1) as i32,
                   "118 frames after the launch frame: 2 + 2*118 = 238");
    }

    /// **The both-frozen screen is two rooms, not two blank consoles.** The state is `0x2f64` = 1
    /// for both with both visitor flags clear (`freeze-death` ds_00034), which is what
    /// `freeze-death/f_014000.jpg` shows: both statues standing in their own rooms. The port's
    /// compose path blanks a console whose spy is a *visitor*, so this state must not.
    #[test]
    fn both_frozen_consoles_draw_their_own_rooms() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let Some(st) = session_dump("freeze-death", "ds_00034.bin") else { return };
        let word = |a: usize| u16::from_le_bytes([st.ds[a], st.ds[a + 1]]);
        assert_eq!((word(0x2F64), word(0x2F66)), (1, 1), "both frozen, neither out");
        assert_eq!((word(0x2F7E), word(0x2F80)), (0, 0), "and neither is a visitor");
        let mut g = game(RoundOpts { island: 6, players: 2, ..RoundOpts::default() });
        g.seed_from_dump(&st, true);
        let mut strips = Vec::new();
        for i in 0..2 {
            let strip = g.strip_for(i, false);
            assert_ne!(strip, [0x55u8; STRIP_LEN], "console {} is not the flat plate", i + 1);
            let inks: std::collections::BTreeSet<u8> = strip.iter().copied().collect();
            assert!(inks.len() > 3, "console {} draws a room ({} inks)", i + 1, inks.len());
            strips.push(strip);
        }
        assert_ne!(strips[0], strips[1], "and they are two different rooms");
        // both statues are drawn, and the pose set is still the death animation's own `0x228`:
        // `0x2EF8` = `0x72` is written *before* `277c`, which overrides it with
        // `word[0x2CC4 + 0x14]` — the statue's drawn shape comes from the record, not the word
        for i in 0..2 {
            assert_eq!(word(round::DS_POSE_SET + 2 * i) as u32, 0x228,
                       "player {}'s pose set, as the dump has it", i + 1);
        }
    }

    /// **The difficulty word is the round's own.** The options screen's difficulty zone steps
    /// `DS:0x31bf` by 2 from 0 to 8 (`CS:1066`), and the round copies it nowhere: it *is*
    /// `0x31bf` in play — measured on `rocket-room-4-hard`, where the owner switched from
    /// difficulty 1 to 5 mid-session: 0 → 8 at t 456.4 s, 261 dumps with 8, 1399 with 0, and the
    /// same island clock (480 s) both times.
    #[test]
    fn the_difficulty_word_reaches_the_round() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { island: 3, players: 2, difficulty: 4, ..RoundOpts::default() });
        let st = g.round_state();
        assert_eq!(u16::from_le_bytes([st.ds[round::DS_DIFF], st.ds[round::DS_DIFF + 1]]), 8,
                   "difficulty 5 is 0x31bf = 8 (`CS:1066` steps 0,2,..,8)");
        assert_eq!(g.data.clock_secs, 840, "and the clock is the island's, not the difficulty's");
        g.difficulty = 0;
        let st = g.round_state();
        assert_eq!(u16::from_le_bytes([st.ds[round::DS_DIFF], st.ds[round::DS_DIFF + 1]]), 0);
    }


    // ---- stage 18c: the map ---------------------------------------------------------------
    /// The eight recorded dumps of `playtest5` whose **own work buffer** (`DS:0x1A92`) holds the
    /// finished map: the window in the dump is byte-identical to the window the engine draws from
    /// the same dump (3960 of 3960 px, measured by `stage18c_frames.py`). The other 97 map dumps
    /// were captured between the map's draw and the console's blit, so their buffer holds the
    /// previous frame's scene; they are covered against the recorded *framebuffer*, not here.
    const MAP_STRIP_DUMPS: [&str; 8] = [
        "ds_00114.bin", "ds_00116.bin", "ds_00195.bin", "ds_00280.bin",
        "ds_00337.bin", "ds_00788.bin", "ds_01266.bin", "ds_01268.bin",
    ];

    fn map_dump(name: &str) -> PathBuf {
        session_dir().join("playtest5").join(name)
    }

    /// A two-console round seeded from a recorded dump — the shape the recorded map frames have
    /// (the original's one-player round still runs both spies, the black one on the computer).
    fn seeded(name: &str) -> Game {
        let path = map_dump(name);
        let st = round::RoundState::from_ds_dump(&path).expect("dump");
        let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
        g.seed_from_dump(&st, true);
        g
    }

    /// The map's plate is **not composed from the level table**: it is one of seven 992-byte
    /// pictures in the image, copied to `DS:0x1052` at round start (`CS:1204`: `bx = word[0x31BB]`,
    /// `si = word[0x72C0+bx]`, `cx = 0x3E0`, `rep movsb`).
    #[test]
    fn the_map_plate_is_the_islands_own_image_blob() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let ds = &exe[DS_FILE_OFF..];
        let mut blobs: Vec<Vec<u8>> = Vec::new();
        for i in 0..7usize {
            let b = round::island_plate(&exe, i).expect("island blob");
            assert_eq!(b.len(), round::MAP_PLATE_LEN, "island {}", i + 1);
            // the two-byte header `0x205D` reads: rows, bytes a row
            assert_eq!((b[0], b[1]), (45, 22), "island {}'s plate header", i + 1);
            let at = u16::from_le_bytes([
                ds[round::DS_ISLAND_PLATE + 2 * i],
                ds[round::DS_ISLAND_PLATE + 2 * i + 1],
            ]) as usize;
            assert_eq!(at, 0x7DAA + 0x3E0 * i, "the table is seven pictures 0x3E0 apart");
            blobs.push(b.to_vec());
        }
        // island-specific: no two islands share a picture
        for i in 1..7 {
            assert_ne!(blobs[i], blobs[0], "island {} has island 1's plate", i + 1);
        }
    }

    /// The round-start copy is the whole of the plate's "composition", and the recording agrees
    /// byte for byte: every recorded map dump holds exactly its island's blob at `DS:0x1052`.
    #[test]
    fn the_composed_plate_is_the_dump_plate() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        for name in MAP_STRIP_DUMPS {
            let ds = std::fs::read(map_dump(name)).expect("dump");
            let island = u16::from_le_bytes([ds[0x31BB], ds[0x31BC]]) as usize / 2;
            let blob = round::island_plate(&exe, island).expect("blob");
            assert_eq!(
                &ds[round::DS_MAP_PLATE..round::DS_MAP_PLATE + round::MAP_PLATE_LEN],
                blob,
                "{name}: the dump's 0x1052 is island {}'s blob", island + 1
            );
        }
    }

    /// **The pixel test of the window itself**: the plate at (44, 5) px of the console's strip,
    /// the trail's legs, one dot per position record and the blinking own-position marker, byte
    /// for byte against the work buffer the game itself left in the same dump.
    #[test]
    fn the_map_window_is_the_recorded_work_buffer() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for name in MAP_STRIP_DUMPS {
            let ds = std::fs::read(map_dump(name)).expect("dump");
            let mut g = seeded(name);
            // the map of the *black* spy: `0x2F4A` in every one of these dumps
            assert!(g.spies[1].map_open, "{name}: the dump has the map up");
            let strip = g.strip_for(1, false);
            let mut diff = 0usize;
            for row in 0..round::MAP_PLATE_ROWS {
                for c in 0..round::MAP_PLATE_BYTES {
                    let i = (round::MAP_ROW + row) * round::STRIP_PITCH + round::MAP_COL + c;
                    let want = ds[round::DS_STRIP + i];
                    if strip[i] != want {
                        diff += 1;
                    }
                }
            }
            assert_eq!(diff, 0, "{name}: {} of 990 window bytes differ", diff);
            // and the same frame's window is empty when the map is down
            g.spies[1].map_open = false;
            let shut = g.strip_for(1, false);
            let mut moved = 0usize;
            for row in 0..round::MAP_PLATE_ROWS {
                let i = (round::MAP_ROW + row) * round::STRIP_PITCH + round::MAP_COL;
                if shut[i..i + round::MAP_PLATE_BYTES] != strip[i..i + round::MAP_PLATE_BYTES] {
                    moved += 1;
                }
            }
            assert_eq!(moved, round::MAP_PLATE_ROWS, "{name}: with the map down every row changes");
        }
    }

    /// `CS:2A87` → `CS:2B6E`: pressing A on slot 12 is **not** an item take. It sets the map's own
    /// word and closes the panel, and the look that follows is the same frame drawn 24 times.
    #[test]
    fn the_map_opens_on_slot_twelve_and_closes_the_panel() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let img = g.exe[DS_FILE_OFF..].to_vec();
        let bit = items::slot_bit(&img, items::SLOT_MAP);
        assert_eq!(bit, 0x0800, "the map slot's own kit bit (`0x2D8C+12`)");
        g.spies[0].kit |= bit;
        g.spies[0].trap_open = true;
        g.spies[0].trap_cursor = items::SLOT_MAP;
        hold(&mut g, FIRE, 26);
        assert!(g.spies[0].map_open, "the map is up");
        assert!(!g.spies[0].trap_open, "and the panel is down: the original never keeps it open");
        assert_eq!(g.spies[0].map_sign, 1, "`0x2A87` writes the constant 1");
        assert_eq!(g.spies[0].carried, CARRY_NONE, "slot 12 is not an item");
    }

    /// `CS:2D22`'s first instruction: any fire press takes the map down and the press is spent —
    /// it does not pick anything up and it does not count as a trapulator tap.
    #[test]
    fn a_fire_press_takes_the_map_down() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.spies[0].map_open = true;
        g.spies[0].map_sign = 1;
        g.spies[0].fire_taps = 0;
        let it = g.test_put(0, 30, 46, 1); // an item under the feet, so a take would be visible
        let _ = it;
        hold(&mut g, FIRE, 26);
        assert!(!g.spies[0].map_open, "the press dismisses the map");
        assert_eq!(g.spies[0].carried, CARRY_NONE, "and does not pick the item up");
        assert!(!g.spies[0].trap_open, "nor does it open the panel");
        assert_eq!(g.spies[0].fire_taps, 0, "`0x2D22` jumps past the tap counter");
    }

    /// The blink: `CS:2CD5` negates the display word once per drawn frame and `CS:2CA3` skips the
    /// own-position marker while it is negative. Two consecutive frames therefore differ in
    /// **39 pixels, all inside the marker's 13 x 9 box** —the number stage 18b measured on the
    /// recordings, reproduced from the engine's own strip.
    #[test]
    fn the_map_blinks_the_own_marker_every_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = seeded("ds_00195.bin");
        assert_eq!(g.spies[1].map_sign, -1, "the dump holds the phase *after* the negate");
        let a = g.strip_for(1, false);
        g.spies[1].map_sign = 1;
        let b = g.strip_for(1, false);
        let (mut n, mut r0, mut r1, mut c0, mut c1) = (0usize, 99usize, 0usize, 99usize, 0usize);
        for row in 0..round::MAP_PLATE_ROWS {
            for c in 0..round::MAP_PLATE_BYTES {
                let i = (round::MAP_ROW + row) * round::STRIP_PITCH + round::MAP_COL + c;
                if a[i] != b[i] {
                    for k in 0..4 {
                        let sh = 6 - 2 * k;
                        if (a[i] >> sh) & 3 != (b[i] >> sh) & 3 {
                            n += 1;
                            r0 = r0.min(row);
                            r1 = r1.max(row);
                            c0 = c0.min(c * 4 + k);
                            c1 = c1.max(c * 4 + k);
                        }
                    }
                }
            }
        }
        assert_eq!(n, 39, "the recorded blink is 39 pixels");
        assert_eq!((r0, r1, c0, c1), (10, 18, 32, 44), "inside the 13 x 9 own-marker box");
    }

    /// `CS:26F7` (round start) and `CS:36D1` (a new cell): five entries, newest first, shifted only
    /// when `row_base[band] + 0x32FF` actually changes.
    #[test]
    fn the_trail_keeps_the_last_five_cells() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        let start = g.map_pos(0);
        assert_eq!(g.spies[0].trail, [start; 5], "`CS:26F7` fills all five");
        g.map_trail_step(0);
        assert_eq!(g.spies[0].trail, [start; 5], "no change, no shift");

        // two gate columns east: `0x32FF` is the column **rounded down to even**, so one column
        // usually does not move the map at all — two always do
        let x = g.spies[0].x();
        let col = g.terrain.col_of(x);
        let next = g.terrain.gate((col + 2).min(11)) + 2;
        g.spies[0].x3 = next * SUB;
        g.spies[0].map_x = (next - 27).max(0);
        let moved = g.map_pos(0);
        assert_ne!(moved, start, "the cell moved");
        g.map_trail_step(0);
        assert_eq!(g.spies[0].trail, [moved, start, start, start, start], "newest first");
        g.map_trail_step(0);
        assert_eq!(g.spies[0].trail[0], moved, "idempotent");
    }

    /// The four dots are the engine's **own** position record (`DS:0x3219`/`0x320F`): a part put
    /// down on screen `s` at cell column `c` draws its dot at the lattice cell `s` and `c / 26`.
    #[test]
    fn the_part_dots_follow_the_engines_position_records() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
        g.spies[1].map_open = true;
        g.spies[1].map_sign = 1;
        let plate_only = g.strip_for(1, false);
        // code 10 (the middle part) on screen 4, cell column 78 -> map column 3
        g.links[2] = cpu::Link { used: true, screen: 4, row: 0, col: 78 };
        let with_dot = g.strip_for(1, false);
        let (mut n, mut rows, mut cols) = (0usize, [99usize, 0usize], [99usize, 0usize]);
        for row in 0..round::MAP_PLATE_ROWS {
            for c in 0..round::MAP_PLATE_BYTES {
                let i = (round::MAP_ROW + row) * round::STRIP_PITCH + round::MAP_COL + c;
                if plate_only[i] != with_dot[i] {
                    for k in 0..4 {
                        let sh = 6 - 2 * k;
                        if (plate_only[i] >> sh) & 3 != (with_dot[i] >> sh) & 3 {
                            n += 1;
                            rows = [rows[0].min(row), rows[1].max(row)];
                            cols = [cols[0].min(c * 4 + k), cols[1].max(c * 4 + k)];
                        }
                    }
                }
            }
        }
        assert!(n > 0, "the dot is drawn");
        // rows and columns are **plate-relative** here. `word[0x15D8 + 4]` = 32 is the lattice row
        // (strip row 32, plate row 27), the dot's bottom is two above it (`sub $0x2`) and the
        // record is three rows tall, so it covers plate rows 22..24. Its column is
        // `word[0x15E2 + 2*(78/26)] + 1` = 23 bytes = px 92..95 of the plate.
        assert_eq!(rows, [22, 24], "screen 4 puts the lattice row on 32");
        assert_eq!(cols, [49, 51], "column 78 / 26 = 3 is plate byte 12 = strip byte 23");

        // and an unplaced record draws nothing
        g.links[2] = cpu::Link::default();
        assert_eq!(g.strip_for(1, false), plate_only, "an unused record is skipped");
    }

    /// `CS:2B85`: the other spy is alive and a console is a visitor's, i.e. both spies are in one
    /// room — the map takes itself down rather than give the room away.
    #[test]
    fn the_map_hides_when_the_spies_share_a_room() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
        g.spies[1].map_open = true;
        g.spies[1].map_sign = 1;
        g.spies[1].blank = false;
        g.spies[0].blank = false;
        hold(&mut g, Keys::default(), 26);
        assert!(g.spies[1].map_open, "apart: the map stays");

        g.spies[1].map_open = true;
        g.spies[0].blank = true; // the visitor's console
        hold(&mut g, Keys::default(), 26);
        assert!(!g.spies[1].map_open, "shared room: the map takes itself down");
    }

    /// The whole window round-trips through the data segment: the display word with its sign, the
    /// spy's map column and his five trail entries are all words of the original's `DS`.
    #[test]
    fn the_map_round_trips_through_the_data_segment() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = seeded("ds_00195.bin");
        let st = g.round_state();
        let w = |a: usize| u16::from_le_bytes([st.ds[a], st.ds[a + 1]]) as i16;
        assert_eq!(w(round::DS_MAP_FLAG + 2), -1, "0x2F4A, the phase the dump holds");
        assert_eq!(w(round::DS_MAP_COL + 2), 4, "0x3301, the black spy's map column");
        for k in 0..5usize {
            let a = round::DS_MAP_TRAIL + 10 + 2 * k;
            assert_eq!(
                u16::from_le_bytes([st.ds[a], st.ds[a + 1]]),
                [76u16, 78, 54, 52, 28][k],
                "0x2DFC+{k}: the black spy's trail",
            );
        }
        // a fresh engine seeded from that segment reproduces the same window
        let mut g2 = game(RoundOpts { players: 2, ..RoundOpts::default() });
        g2.seed_from_dump(&st, true);
        assert!(g2.spies[1].map_open);
        assert_eq!(g2.spies[1].trail, g.spies[1].trail);
        // The segment carries the phase the **next draw** uses (`map_sign`), while a recorded dump
        // carries the phase `CS:2CD5` left behind *after* the draw — so seeding from a segment the
        // engine itself wrote flips the sign once. Everything else round-trips untouched, and with
        // the phase aligned the two windows are the same bytes.
        assert_eq!(g2.spies[1].map_sign, -g.spies[1].map_sign);
        g2.spies[1].map_sign = -g2.spies[1].map_sign;
        assert_eq!(g2.strip_for(1, false), g.strip_for(1, false));
    }


    /// `CS:07cd`: while the map is up the **direction keys do nothing** (and the spy is not
    /// stopped when they are held — `CS:07c2`'s stop only runs on an empty keyboard), while a fire
    /// press still dismisses the window.
    #[test]
    fn the_map_ignores_the_direction_keys() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.spies[0].map_open = true;
        g.spies[0].map_sign = 1;
        let (x, y) = (g.spies[0].x(), g.spies[0].y());
        hold(&mut g, RIGHT, 26 * 4);
        assert!(g.spies[0].map_open, "the map is still up");
        assert_eq!((g.spies[0].x(), g.spies[0].y()), (x, y), "he did not walk");
        assert_eq!(g.spies[0].frame, STAND_FRAME, "and he is not left mid-stride");

        // the fire press still takes it down
        hold(&mut g, FIRE, 26);
        assert!(!g.spies[0].map_open);
        // ... and then the keys work again
        hold(&mut g, RIGHT, 26 * 2);
        assert_ne!(g.spies[0].x(), x, "moving again");
    }

    // ---- stage 18g: the cues are fired from the original's own call sites --------------------

    /// Every div the sequencer wrote since `from` (the engine's own stream, one note at a time).
    fn sound_divs(g: &mut Game, ticks: usize) -> Vec<u16> {
        let mut v = Vec::new();
        for _ in 0..ticks {
            g.tick(Input { p1: NOKEYS, p2: NOKEYS });
            if let Some(d) = g.sound_out.div {
                v.push(d);
            }
        }
        v
    }

    /// `CS:38CA..38D7` — a pickup plays `0xA15A`, one slide at div 2337 (510.6 Hz) for 117 ms.
    #[test]
    fn a_pickup_plays_the_select_cue() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        put_underfoot(&mut g, 9); // a loose rocket part
        fire_frames(&mut g, FIRE, 1);
        assert_ne!(g.spies[0].carried, CARRY_NONE, "he took it");
        assert_eq!(g.cue_playing(), Some(sound::cue::SELECT));
        // and the note itself: div 2337 written to the PIT on the tick the cue was taken
        assert_eq!(g.sound_out.div, None, "a game frame is not a sequencer tick");
        let divs = sound_divs(&mut g, 4);
        assert_eq!(divs.first(), Some(&2337), "the click: 1193182/2337 = 510.6 Hz");
    }

    /// `CS:2A6C` — the trapulator's A on an empty slot buzzes; on a held slot it clicks.
    #[test]
    fn the_trapulator_clicks_on_a_held_slot_and_the_cursor_clicks_too() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.spies[0].trap_open = true;
        g.spies[0].trap_cursor = 2;     // slot 1 = the saw
        g.spies[0].kit |= kit_bit(3);
        g.spies[0].fire_prev = false;
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.spies[0].carried, 3, "the saw came out");
        assert_eq!(g.cue_playing(), Some(sound::cue::SELECT));

        // the cursor: one click per step, div 4033 (296 Hz)
        let mut g = plain();
        g.spies[0].trap_open = true;
        g.spies[0].trap_cursor = 0;
        g.spies[0].cursor_delay = 0;
        // the frame runs on the 26th tick; the cue it writes is played on the tick after that
        hold(&mut g, Keys { right: true, ..NOKEYS }, FRAME_TICKS as usize);
        let cursor_divs = sound_divs(&mut g, 2);
        assert_eq!(cursor_divs, vec![4033], "the cursor click is div 4033: {cursor_divs:?}");
        assert_eq!(g.spies[0].trap_cursor, 2, "and it moved one slot");
    }

    /// `CS:2A6C` again, the buzz: the kit bit is clear.
    #[test]
    fn an_empty_trapulator_slot_plays_the_denied_buzz() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        g.spies[0].trap_open = true;
        g.spies[0].kit = 0;              // nothing in the kit at all
        g.spies[0].trap_cursor = 2;
        g.spies[0].fire_prev = false;
        fire_frames(&mut g, FIRE, 1);
        assert_eq!(g.cue_playing(), Some(sound::cue::PANEL_DENIED));
        assert_eq!(g.spies[0].carried, CARRY_NONE, "and nothing came out");
    }

    /// `CS:30C9..30DC` — the snowball's 24-note noise.
    #[test]
    fn a_snowball_plays_its_noise() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts { players: 2, start_screen: Some(4), start_x: Some(100),
                                     start_x2: Some(120), start_y: Some(44),
                                     ..RoundOpts::default() });
        g.levels = [[0u16; 12]; 10];
        g.spies[0].blank = true;
        fire_frames(&mut g, Keys { left: true, fire_b: true, ..NOKEYS }, 1);
        assert!(g.spies[0].ball.is_some(), "B plus a direction throws");
        assert_eq!(g.cue_playing(), Some(sound::cue::SNOWBALL));
        let divs = sound_divs(&mut g, 6);
        assert_eq!(divs.first(), Some(&6818), "the throw's first note");
    }

    /// `CS:2490..249D` and `CS:24C0..24CD` — a spilled bucket slips, a hole falls, and the two
    /// play *different* glissandi: rising for the spill, falling for the hole.
    #[test]
    fn the_two_hazards_play_the_two_glissandi() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        put_underfoot(&mut g, 0x0D); // the spill
        fire_frames(&mut g, NOKEYS, 1);
        assert_eq!(g.cue_playing(), Some(sound::cue::SLIP_RISE));
        let mut g = plain();
        put_underfoot(&mut g, 0x0E); // the hole
        fire_frames(&mut g, NOKEYS, 1);
        assert_eq!(g.cue_playing(), Some(sound::cue::FALL));
    }

    /// The queue is one word deep: a second cue in the same frame is dropped, which is what the
    /// game's `cmpw $2,0x34A0 / jg` guard is for.
    #[test]
    fn only_one_cue_sounds_at_a_time() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        assert!(g.cue(sound::cue::SNOWBALL));
        assert!(!g.cue(sound::cue::SELECT), "the second cue is dropped");
        assert_eq!(g.sound.pending, sound::cue::SNOWBALL);
    }

    /// `CS:2517..2524` — the icicle's blast (`0x14` underfoot) plays the explosion cue.
    #[test]
    fn the_icicle_blast_plays_the_explosion() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = plain();
        put_underfoot(&mut g, 0x14);
        fire_frames(&mut g, NOKEYS, 1);
        assert_eq!(g.cue_playing(), Some(sound::cue::EXPLOSION));
    }

    // ---- stage 20a: the DAMAGE model ----------------------------------------------------------
    //
    // `FUN_26c0_21bd` is the heat step (`CS:21d6..2200`) *and* the bar's only writer (`0x3455` +
    // `0x45b0`). It has no sound call in it, so the negative `AX` at six of its call sites is
    // damage, not a sound id: `STAGE13A/13B/16B` read them as sound and left the damage out.
    // Every constant is pinned twice below — once against the executable's own `mov $imm,%ax`,
    // once against the recorded heat word — plus a replay from the dump for each source.

    /// `(cs offset of the call, the AX it is called with)` for every `call 0x21bd` in
    /// `CS:0000..0x4700`, read out of the stock image the way `recon/round/disasm_cs.py` does.
    fn heat_call_sites() -> Vec<(usize, i32)> {
        let exe = exe();
        let cs = &exe[0x16E00..0x16E00 + 0x4700];
        let mut out = Vec::new();
        for off in 0..cs.len().saturating_sub(3) {
            if cs[off] != 0xE8 {
                continue;
            }
            let rel = u16::from_le_bytes([cs[off + 1], cs[off + 2]]) as i16 as i32;
            if off as i32 + 3 + rel != 0x21BD {
                continue;
            }
            let mut ax = None;
            for back in 3..11usize {
                if off >= back && cs[off - back] == 0xB8 {
                    let v = u16::from_le_bytes([cs[off - back + 1], cs[off - back + 2]]);
                    ax = Some(v as i16 as i32);
                    break;
                }
            }
            out.push((off, ax.expect("`mov $imm,%ax` before the call")));
        }
        out
    }

    /// **The damage table is the executable's** — each site's `AX`, and the cold it means (two per
    /// step, `CS:21fb`). The spill's branch is asserted to have **no** `21bd` call, which is the
    /// zero that matters: `CS:2473`..`24a6` clears the byte and starts the animations only.
    #[test]
    fn the_damage_table_is_the_executables_immediates() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sites = heat_call_sites();
        let ax_at = |addr: usize| -> i32 {
            sites.iter().find(|(o, _)| *o == addr).map(|(_, a)| *a)
                .unwrap_or_else(|| panic!("no `call 0x21bd` at CS:{addr:04x}"))
        };
        // (site, AX, the constant it must mean)
        for (addr, steps, name) in [
            (0x075Eusize, -1i32, "the base drain"),
            (0x1E4B, -3, "the ice terrain's slip"),
            (0x1EC9, -1, "deep snow"),
            (0x1F02, -1, "water"),
            (0x1F27, 1, "the fire"),
            (0x200E, 1, "the igloo floor"),
            (0x24B7, -DAMAGE_HOLE_STEPS, "the saw hole"),
            (0x2514, -DAMAGE_ICICLE_STEPS, "the icicle"),
            (0x2E24, -DAMAGE_BLAST_STEPS, "the plunger"),
            (0x30F1, -DAMAGE_SNOWBALL_STEPS, "the snowball"),
            (0x37E1, -DAMAGE_BLAST_STEPS, "a tapped stick"),
        ] {
            assert_eq!(ax_at(addr), steps, "{name}: CS:{addr:04x}'s AX");
        }
        // the spill: no heat call anywhere in `CS:2473..24ac`, i.e. `DAMAGE_SPILL_STEPS` is 0
        assert!(
            !sites.iter().any(|(o, _)| (0x2473..0x24AC).contains(o)),
            "the spill branch calls 21bd: the zero is not a zero"
        );
        assert_eq!(DAMAGE_SPILL_STEPS, 0);
        // and the measured deltas, cold units, are two per step
        assert_eq!(DAMAGE_HOLE_STEPS * 2, 6);
        assert_eq!(DAMAGE_ICE_STEPS * 2, 6);
        assert_eq!(DAMAGE_ICICLE_STEPS * 2, 10);
        assert_eq!(DAMAGE_BLAST_STEPS * 2, 10);
        assert_eq!(DAMAGE_SNOWBALL_STEPS * 2, 4);
    }

    /// **The saw hole costs 6 cold** — `CS:24b1` clears the byte, `CS:24b4` adds three steps, and
    /// only then does `CS:24ba` write state 0x10. Both spies, and `0x2F54` is cleared by the step's
    /// tail (`CS:2231`).
    #[test]
    fn a_saw_hole_costs_six_cold_and_restarts_the_base_counter() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for victim in 0..2 {
            let mut g = two_at(0, 4, 120, 44, 150);
            let a = put_underfoot_for(&mut g, victim, items::OBJ_HOLE);
            g.spies[victim].base_t = 60; // a mid-count base counter, as in the recording
            let before = g.spies[victim].heat;
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.spies[victim].heat, before + 6,
                       "P{}: the hole is three steps of two", victim + 1);
            assert_eq!(g.spies[victim].base_t, 0, "`CS:2231` clears 0x2F54");
            assert_eq!(g.spies[1 - victim].heat, HEAT_FULL, "the other spy pays nothing");
            assert_eq!(g.ds[a], 0, "the hole is consumed either way");
            assert_eq!(g.spies[victim].anim.map(|x| x.state), Some(round::STATE_FALL));
        }
    }

    /// **The icicle costs 10 cold** — `CS:2511` sits between the three cleared bytes / the host's
    /// `0x2F44` flag and the knock-out's state word.
    #[test]
    fn the_icicle_costs_ten_cold() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for victim in 0..2 {
            let mut g = two_at(0, 4, 120, 34, 150);
            let a = put_underfoot_for(&mut g, victim, items::OBJ_ICICLE);
            g.ds[a - 1] = items::OBJ_ICICLE;
            g.ds[a + 1] = items::OBJ_ICICLE;
            let before = g.spies[victim].heat;
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.spies[victim].heat, before + 10, "five steps of two");
            assert_eq!(g.spies[victim].anim.map(|x| x.state), Some(round::STATE_KNOCKOUT));
            assert_eq!(g.spies[1 - victim].heat, HEAT_FULL);
        }
    }

    /// **The plunger costs the victim 10 cold and the firer nothing** — `CS:2e21`, inside the
    /// branch that found my tag under him (`CS:2e10`).
    #[test]
    fn the_plunger_costs_the_victim_ten_cold_and_the_firer_nothing() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for actor in 0..2 {
            let victim = 1 - actor;
            let mut g = two_at(0, 4, 120, 44, 140);
            planted(&mut g, victim, actor);
            g.set_carried(actor, plunger_of(actor));
            let (hv, hf) = (g.spies[victim].heat, g.spies[actor].heat);
            press_button_b(&mut g, actor);
            assert_eq!(g.spies[victim].heat, hv + 10, "the blast is five steps of two");
            assert_eq!(g.spies[actor].heat, hf, "pulling the plunger costs the firer nothing");
            assert_eq!(g.spies[victim].anim.map(|x| x.state), Some(round::STATE_BLAST));
        }
    }

    /// **A dud costs nothing.** The `21bd` call sits after the tag test, so a plunger with nothing
    /// of mine under him leaves both heat words alone.
    #[test]
    fn a_dud_plunger_costs_no_heat() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for actor in 0..2 {
            let victim = 1 - actor;
            let mut g = two_at(0, 4, 120, 44, 140);
            g.set_carried(actor, plunger_of(actor));
            let (hv, hf) = (g.spies[victim].heat, g.spies[actor].heat);
            press_button_b(&mut g, actor);
            assert_eq!((g.spies[victim].heat, g.spies[actor].heat), (hv, hf));
        }
    }

    /// **Tapping the other spy's stick costs the tapper 10 cold** — `CS:37de` is the tapper's own
    /// `21bd` call, the mirror image of the plunger's.
    #[test]
    fn a_tapped_stick_costs_the_tapper_ten_cold() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for owner in 0..2 {
            let victim = 1 - owner;
            let mut g = two_at(0, 4, 120, 44, 140);
            planted(&mut g, victim, owner);
            let (hv, ho) = (g.spies[victim].heat, g.spies[owner].heat);
            tap(&mut g, victim);
            assert_eq!(g.spies[victim].heat, hv + 10, "the tapper is the victim");
            assert_eq!(g.spies[owner].heat, ho, "the owner only laughs");
        }
    }

    /// **A snowball hit costs 4 cold** — `CS:30ee`, with `SI` switched to the victim at `CS:30e9`.
    /// Every facing pair and both throwers, because the damage must not depend on the animation the
    /// sum picks (`0x16` or `0x18`).
    #[test]
    fn a_snowball_hit_costs_four_cold() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let cases = [
            (RIGHT, 120, 126, false),
            (RIGHT, 120, 126, true),
            (LEFT, 126, 120, false),
            (LEFT, 126, 120, true),
        ];
        for (dir, tx, vx_, vmirror) in cases {
            for thrower in 0..2usize {
                let victim = 1 - thrower;
                let (x1, x2) = if thrower == 0 { (tx, vx_) } else { (vx_, tx) };
                let mut g = two_at(0, 4, x1, 44, x2);
                frame2(&mut g, NOKEYS, NOKEYS);
                clear_pickups(&mut g, thrower);
                g.spies[victim].mirror = vmirror;
                let before = g.spies[victim].heat;
                let thrower_before = g.spies[thrower].heat;
                throw_frame(&mut g, thrower, dir);
                for _ in 0..5 {
                    frame2(&mut g, NOKEYS, NOKEYS);
                }
                assert!(matches!(g.spies[victim].anim.map(|a| a.state),
                                  Some(round::STATE_KNOCK_FACE) | Some(round::STATE_KNOCK_BACK)),
                        "the hit must land for the heat to be the hit's");
                assert_eq!(g.spies[victim].heat, before + 4,
                           "thrower {thrower}, victim facing west {vmirror}");
                assert_eq!(g.spies[thrower].heat, thrower_before,
                           "throwing costs nothing (`STAGE16B` §1.1)");
            }
        }
    }

    /// **The split the owner's report turns on**: a *spilt* patch costs **no** heat (`CS:2473` has
    /// no `21bd` call) while the **ice terrain's** slip costs **6** (`CS:1e48`). Both put the spy in
    /// the same state `0x10`, which is why they look alike on screen and must be told apart here.
    #[test]
    fn a_spilt_patch_costs_no_heat_and_the_ice_terrain_costs_six() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_at(0, 4, 120, 44, 150);
        put_underfoot_for(&mut g, 0, items::OBJ_SPILL);
        let before = g.spies[0].heat;
        frame2(&mut g, NOKEYS, NOKEYS);
        assert_eq!(g.spies[0].heat, before, "the spill's branch never touches `0x2F58`");
        assert_eq!(g.spies[0].anim.map(|x| x.state), Some(round::STATE_SLIP));
        assert_eq!(g.spies[1].heat, HEAT_FULL);

        // the ice terrain: the same state, and six cold — via the roll the existing ice test uses
        let ice = find_code(&game(RoundOpts { island: 1, start_screen: Some(12), ..RoundOpts::default() }),
                            terrain::T_ICE, 44)
            .expect("an ice cell");
        let mut g = two_at(1, ice.0, ice.1, 44, ice.1 + 40);
        let before = g.spies[0].heat;
        let mut slipped = false;
        for _ in 0..80 {
            frame2(&mut g, RIGHT, NOKEYS);
            if g.spies[0].anim.map(|a| a.state) == Some(round::STATE_FALL) {
                slipped = true;
                break;
            }
        }
        assert!(slipped, "the ice roll must fire within 80 moving frames");
        assert_eq!(g.spies[0].heat, before + 6, "`CS:1e48`: three steps of two");
    }

    /// **The clamp is the recording's, not a guess**: the step refuses to pass `0x3D` (`CS:21f4`),
    /// so a snowball into a spy at 61 moves his word by **2**, not 4 — exactly what `playtest5`
    /// ds_01633 shows (61 -> 63 on a `0x16`).
    #[test]
    fn damage_stops_at_the_freeze_word() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_at(0, 4, 120, 44, 126);
        frame2(&mut g, NOKEYS, NOKEYS);
        clear_pickups(&mut g, 0);
        g.spies[1].heat = HEAT_FREEZE_AT;
        throw_frame(&mut g, 0, RIGHT);
        for _ in 0..5 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        assert!(matches!(g.spies[1].anim.map(|a| a.state),
                          Some(round::STATE_KNOCK_FACE) | Some(round::STATE_KNOCK_BACK)));
        assert_eq!(g.spies[1].heat, 63, "61 + 2, the add refused above 0x3D");
    }

    /// **No double hit, and the guard is the code's, not a flag on the heat step.** A ball refuses
    /// a victim already in `0x32CF >= 0x0C` (`CS:30bf`: a slip, a fall, a laugh, a blast), so a spy
    /// blown up for 3.6 s cannot be snowballed again inside it and his word stays where the blast
    /// left it. The same setup with an idle victim *does* land (the `a_snowball_hit_costs_four_cold`
    /// cases), so the miss is the guard and not the geometry.
    #[test]
    fn a_spy_already_in_a_hazard_state_is_not_hit_twice() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_at(0, 4, 114, 44, 120);
        frame2(&mut g, NOKEYS, NOKEYS);
        planted(&mut g, 1, 0);
        g.set_carried(0, plunger_of(0));
        press_button_b(&mut g, 0);
        assert_eq!(g.spies[1].anim.map(|a| a.state), Some(round::STATE_BLAST));
        let after_blast = g.spies[1].heat;
        assert_eq!(after_blast, 3 + 10, "the blast itself");
        g.set_carried(0, CARRY_NONE);
        throw_frame(&mut g, 0, RIGHT);
        for _ in 0..5 {
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        assert_eq!(g.spies[1].act_state, round::STATE_BLAST,
                   "`CS:30bf` refuses a victim in 0x0C or above");
        assert_eq!(g.spies[1].heat, after_blast, "and his word does not move again");
    }

    /// `dump name -> recording clock`, from the session's own `state.csv`.
    fn dump_monos(session: &str) -> std::collections::HashMap<String, f64> {
        let p = session_dir().join(session).join("state.csv");
        let Ok(txt) = std::fs::read_to_string(&p) else { return Default::default() };
        let mut lines = txt.lines();
        let head: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
        let (Some(im), Some(id)) = (head.iter().position(|c| *c == "mono"),
                                    head.iter().position(|c| *c == "dump")) else {
            return Default::default();
        };
        let mut m = std::collections::HashMap::new();
        for l in lines {
            let f: Vec<&str> = l.split(',').collect();
            if let (Some(a), Some(b)) = (f.get(im), f.get(id)) {
                if let Ok(t) = a.parse::<f64>() {
                    m.insert((*b).to_string(), t);
                }
            }
        }
        m
    }

    /// **The measured model, source by source** — every animation birth in the corpus whose cause
    /// is one of the six damage sources, with the heat delta across the pair asserted against the
    /// code's number. The pairs are restricted to those the recording clock puts within 0.5 s of
    /// each other, so no base drain (`0x8A` frames) can be hiding in the delta.
    ///
    /// A *fall* (state `0x10`) is a hole when the before dump has `0x0E` under the spy's feet and
    /// the ice otherwise: `CS:24ba` and `CS:1e64` are the only two sites in the image that write
    /// `0x10`, and `1e64` is the ice branch's own continuation (`1e53` jumps into it).
    #[test]
    fn every_recorded_damage_event_costs_the_codes_cold() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = [
            "audio-smoke", "audio-smoke-2", "iceaxe", "m1-fix-960", "m3-heat-90s", "playtest",
            "playtest1", "playtest2", "playtest5", "rocket-room-2", "rocket-room-3-hard",
            "rocket-room-4-hard", "trapulator-island4",
        ];
        let mut seen: std::collections::BTreeMap<&'static str, usize> = Default::default();
        let mut bad: Vec<String> = Vec::new();
        for ses in sessions {
            let ds = dumps_of(ses);
            let mono = dump_monos(ses);
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                let (n0, n1) = (w[0].file_name().unwrap().to_string_lossy().to_string(),
                                w[1].file_name().unwrap().to_string_lossy().to_string());
                let dt = match (mono.get(&n0), mono.get(&n1)) {
                    (Some(x), Some(y)) => y - x,
                    _ => continue,
                };
                if !(0.0..=0.5).contains(&dt) {
                    continue;
                }
                for i in 0..2usize {
                    let o = 2 * i;
                    let act_a = w16(&a, 0x32CB + o);
                    let act_b = w16(&b, 0x32CB + o);
                    if act_a == act_b || w16(&a, 0x2F64 + o) >= 2 {
                        continue;
                    }
                    let (name, want): (&str, i32) = match act_b {
                        0x0E => ("spill", 0),
                        0x10 => {
                            let band = w16(&a, 0x329F + o);
                            let x = w16(&a, 0x329B + o) as i32;
                            let y = w16(&a, 0x3297 + o) as i32;
                            let hole = items::cell_addr(&a, band, x, y)
                                .map(|c| a[c] == items::OBJ_HOLE)
                                .unwrap_or(false);
                            if hole { ("hole", 6) } else { ("ice", 6) }
                        }
                        0x12 => ("blast", 10),
                        0x16 | 0x18 => ("snowball", 4),
                        0x1A => ("icicle", 10),
                        _ => continue,
                    };
                    let h0 = w16(&a, 0x2F58 + o);
                    let h1 = w16(&b, 0x2F58 + o);
                    let delta = (h1 as i32) - (h0 as i32);
                    *seen.entry(name).or_default() += 1;
                    // Two recorded pairs are the model plus one of the code's own neighbours, and
                    // both are named rather than tolerated:
                    //  * a word already at 0x3D takes only 2 of a 4-cold hit (`CS:21f4` refuses to
                    //    pass 0x3D) — `playtest5` ds_01633;
                    //  * a base counter at 0x89 adds its own 2 on the same frame (`CS:0753`) —
                    //    `rocket-room-3-hard` ds_01094, whose dump carries `0x2F54 = 137`.
                    let clamped = h0 == HEAT_FREEZE_AT && delta == want - 2;
                    let base_tick = delta == want + 2 && (name == "hole" || name == "ice");
                    if delta != want && !clamped && !base_tick {
                        bad.push(format!("{ses}/{n0}->{n1} P{} {name}: {h0} -> {h1}",
                                         i + 1));
                    }
                }
            }
        }
        eprintln!("stage 20a measured damage births: {seen:?}");
        assert!(bad.is_empty(), "{} deltas off the model: {bad:#?}", bad.len());
        for (name, min) in [("snowball", 25usize), ("ice", 6), ("hole", 3), ("blast", 1),
                            ("icicle", 2), ("spill", 2)] {
            assert!(seen.get(name).copied().unwrap_or(0) >= min,
                    "{name}: only {:?} recorded events", seen.get(name));
        }
    }

    /// **The hole and the icicle replay their heat word from the dump**: seed the engine from the
    /// before dump, run the one frame in which the hazard bites, and the victim's word must be the
    /// after dump's own value.
    #[test]
    fn every_recorded_hazard_bite_replays_its_heat_word() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["iceaxe", "m3-heat-90s", "playtest1", "playtest2", "playtest5",
                        "rocket-room-2", "rocket-room-3-hard", "trapulator-island4"];
        let mut tried = 0usize;
        let mut bad = Vec::new();
        for ses in sessions {
            let ds = dumps_of(ses);
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for victim in 0..2usize {
                    let o = 2 * victim;
                    if w16(&a, 0x2F64 + o) >= 2 {
                        continue;
                    }
                    let band = w16(&a, 0x329F + o);
                    let x = w16(&a, 0x329B + o) as i32;
                    let y = w16(&a, 0x3297 + o) as i32;
                    let Some(cell) = items::cell_addr(&a, band, x, y) else { continue };
                    let byte = a[cell];
                    if !matches!(byte, items::OBJ_HOLE | items::OBJ_ICICLE) || b[cell] == byte {
                        continue;
                    }
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    for i in 0..2 {
                        g.spies[i].anim = None;
                        g.spies[i].pose_raised = false;
                    }
                    g.hatch.other = None;
                    let before = g.spies[victim].heat;
                    frame2(&mut g, NOKEYS, NOKEYS);
                    tried += 1;
                    let rec = w16(&b, 0x2F58 + o);
                    if g.spies[victim].heat != rec {
                        bad.push(format!("{ses}/{w:?} P{} {byte:#04x}: port {} rec {} (was {})",
                                         victim + 1, g.spies[victim].heat, rec, before));
                    }
                }
            }
        }
        eprintln!("stage 20a: {tried} recorded hazard bites replayed on the heat word");
        assert!(tried >= 5, "only {tried} bites found");
        assert!(bad.is_empty(), "{} replays disagree: {bad:#?}", bad.len());
    }

    /// **A recorded detonation replays its victim's heat word**: seed from the before dump, pull
    /// the plunger like the before dump's own words imply, and the word must be the after dump's.
    #[test]
    fn every_recorded_detonation_replays_the_victims_heat_word() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["playtest", "rocket-room-2", "rocket-room-4-hard"];
        let mut tried = 0usize;
        let mut bad = Vec::new();
        for ses in sessions {
            let ds = dumps_of(ses);
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for victim in 0..2usize {
                    let o = 2 * victim;
                    let firer = 1 - victim;
                    let turn = |d: &[u8], i: usize| w16(d, 0x32CB + 2 * i);
                    if turn(&b, victim) != 0x12 || turn(&a, victim) == 0x12 || turn(&a, firer) != 0 {
                        continue;
                    }
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    for i in 0..2 {
                        g.spies[i].anim = None;
                        g.spies[i].pose_raised = false;
                    }
                    g.hatch.other = None;
                    press_button_b(&mut g, firer);
                    tried += 1;
                    let rec = w16(&b, 0x2F58 + o);
                    if g.spies[victim].heat != rec {
                        bad.push(format!("{ses} P{}: port {} rec {}", victim + 1,
                                         g.spies[victim].heat, rec));
                    }
                }
            }
        }
        eprintln!("stage 20a: {tried} recorded detonations replayed on the heat word");
        assert!(tried >= 2, "only {tried} detonations found");
        assert!(bad.is_empty(), "{} replays disagree: {bad:#?}", bad.len());
    }

    /// **A recorded snowball hit replays its victim's heat word** — the same discovery stage 16b's
    /// knock-out replay uses (`0x32CF` turning into `0x16`/`0x18` with a ball of the other spy's in
    /// flight), with the ball given up to six frames to land.
    #[test]
    fn every_recorded_snowball_hit_replays_the_victims_heat_word() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let sessions = ["audio-smoke-2", "m1-fix-960", "playtest5"];
        let mut tried = 0usize;
        let mut bad = Vec::new();
        for ses in sessions {
            let ds = dumps_of(ses);
            for w in ds.windows(2) {
                let (Ok(a), Ok(b)) = (std::fs::read(&w[0]), std::fs::read(&w[1])) else { continue };
                if a.len() < 0x10000 || b.len() < 0x10000 {
                    continue;
                }
                for victim in 0..2usize {
                    let o = 2 * victim;
                    let vo = 2 * (1 - victim);
                    let run = w16(&b, 0x32CF + o);
                    if !matches!(run, 0x16 | 0x18) || w16(&a, 0x32CF + o) == run {
                        continue;
                    }
                    if w16(&a, 0x2F40 + vo) == 0 {
                        continue;
                    }
                    let st = round::RoundState::from_ds_dump(&w[0]).unwrap();
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    g.hatch.other = None;
                    let mut landed = false;
                    for _ in 0..6 {
                        frame2(&mut g, NOKEYS, NOKEYS);
                        if matches!(g.spies[victim].anim.map(|x| x.state),
                                    Some(round::STATE_KNOCK_FACE) | Some(round::STATE_KNOCK_BACK)) {
                            landed = true;
                            break;
                        }
                        if g.spies[1 - victim].ball.map(|x| x.flying) != Some(true) {
                            break;
                        }
                    }
                    if !landed {
                        continue;
                    }
                    tried += 1;
                    let rec = w16(&b, 0x2F58 + o);
                    if g.spies[victim].heat != rec {
                        bad.push(format!("{ses} P{}: port {} rec {}", victim + 1,
                                         g.spies[victim].heat, rec));
                    }
                }
            }
        }
        eprintln!("stage 20a: {tried} recorded snowball hits replayed on the heat word");
        assert!(tried >= 10, "only {tried} hits replayed");
        assert!(bad.is_empty(), "{} replays disagree: {bad:#?}", bad.len());
    }

    // ---- stage 22a: the terrain dispatch runs per **pixel step**, not per frame -------------

    /// Walk `g`'s spy 0 east until his view scrolls, which is the condition the original's loop
    /// re-enters on (`CS:0a41`..`0a49`), and leave him just past the dead zone so every frame from
    /// then on is two passes. Returns the frame count spent getting there.
    fn walk_until_two_passes(g: &mut Game) -> usize {
        let east = Keys { right: true, ..NOKEYS };
        let mut n = 0;
        while g.spies[0].map_x == 9 && n < 40 {
            frame2(g, east, NOKEYS);
            n += 1;
        }
        assert!(n < 40, "the view never scrolled");
        n
    }

    /// `CS:0944..0979` is inside the loop body, not above it: the original probes
    /// `FUN_26c0_366e` and dispatches `FUN_26c0_1e0f` once **per pixel step**, and `CS:0a44`'s
    /// `jmp 0x7cd` sends a scrolling frame round again. So a frame that covers two units ticks the
    /// shared counter twice.
    ///
    /// Recorded: every dump of `playtest5`'s water walk (21555.4 .. 21561.1, screen 4, y 38) has
    /// `0x2F60` exactly **2 x** `0x2F54` — the terrain counter runs twice the frame counter for as
    /// long as the spy walks with the view scrolling (20 consecutive dumps).
    #[test]
    fn the_terrain_dispatch_runs_on_every_pixel_step_not_once_a_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = untimed(two_at(0, 4, 29, 44, 90));
        let east = Keys { right: true, ..NOKEYS };
        walk_until_two_passes(&mut g);
        let x0 = g.spies[0].x();
        let m0 = g.spies[0].map_x;
        g.spies[0].terrain_t = 0;
        frame2(&mut g, east, NOKEYS);
        assert_eq!(g.spies[0].x(), x0 + 2, "a scrolling frame covers two units");
        assert_ne!(g.spies[0].map_x, m0, "and it scrolled");
        assert_eq!(g.spies[0].terrain_t, 2, "two pixel steps = two dispatches");
    }

    /// The **parked** case, which is the one the recordings can be checked on: with no key held the
    /// body runs once a frame (`CS:07c7` → `CS:0836` → `CS:0944`), so the twelve-pass water counter
    /// is twelve *frames* and the nine-pass deep-snow counter nine — the port's own frame seconds.
    ///
    /// Measured, the other way round: the recorded `0x2F60` never reaches its period in any dump —
    /// water's highest value is **11**, deep snow's **8**, the igloo floor's **20** — and the value
    /// it does reach is what pins the period (see `the_recorded_terrain_counter_stays_under_its_period`).
    #[test]
    fn a_spy_standing_in_the_water_steps_cold_every_twelve_frames() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = untimed(two_at(0, 4, 29, 44, 90));
        assert_eq!(g.terrain.code(&g.levels, 4, 29, 44), Some(0x01), "the level-8 ocean band");
        for f in 0..11 {
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.spies[0].terrain_t, f + 1, "one pass a frame while parked");
            assert_eq!(g.spies[0].heat, 3, "eleven frames is not yet a step");
        }
        frame2(&mut g, NOKEYS, NOKEYS);
        assert_eq!(g.spies[0].heat, 5, "the twelfth pass is");
    }

    /// The same twelve passes, walked: with the view scrolling the frame covers two units, so the
    /// water drains at **six frames a step** — twice the parked rate. This is the recorded spacing:
    /// `playtest5` 21555.9 → 21561.1 climbs 5 → 21 in eight steps of 0.74 s (6.8 frames), with the
    /// counter at two a frame throughout.
    #[test]
    fn a_spy_walking_the_water_steps_cold_twice_as_fast_as_one_standing_in_it() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = untimed(two_at(0, 4, 29, 44, 90));
        let east = Keys { right: true, ..NOKEYS };
        walk_until_two_passes(&mut g);
        let in_water = |g: &Game| matches!(g.terrain.code(&g.levels, 4, g.spies[0].x(), 44),
                                           Some(0x01) | Some(0x17));
        assert!(in_water(&g), "in the band at x {}", g.spies[0].x());
        // count the passes the body actually makes, from the shared counter itself; the cell is
        // water from x 25 to x 52 on this row, so eight passes is as far as the walk may go
        g.spies[0].terrain_t = 4;
        g.spies[0].heat = 30;
        let (mut passes, mut last, mut frames) = (0u16, 4u16, 0u16);
        while g.spies[0].heat == 30 {
            frame2(&mut g, east, NOKEYS);
            frames += 1;
            assert!(frames < 8, "no step after {frames} frames");
            assert!(in_water(&g), "still in the water at x {}", g.spies[0].x());
            let t = g.spies[0].terrain_t;
            passes += if t >= last { t - last } else { 12 - last + t };
            last = t;
        }
        assert_eq!(passes, 8, "eight passes from the counter's 4 = the twelfth, walk or stand");
        assert_eq!(g.spies[0].heat, 32);
        assert_eq!(frames, 4, "two passes a frame: four frames, not twelve");
    }

    /// `CS:1eb2..1ec6`: the deep-snow counter is the only one of the four whose compare is `jg`
    /// (`7f 03`), not `jge` — so the step lands on the **ninth** pass, not the eighth. The port had
    /// it at eight (`>= 8`) since stage 14, which made deep snow 12 % hotter than the original.
    ///
    /// Measured, without any model in the way: `DS:0x2F60` reads **8** in recorded dumps whose
    /// terrain code is `2` (`playtest4` ds at 20985.73 and 20989.63, `iceaxe` at 488922.14). A
    /// counter that steps at 8 can never be seen at 8 — it is zeroed in the same pass.
    #[test]
    fn deep_snow_steps_cold_on_the_ninth_pass_not_the_eighth() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let snow = x_on_code(&game(RoundOpts { island: 1, start_screen: Some(4), ..RoundOpts::default() }),
                             4, 0x02, 44)
            .expect("island 2 has a deep-snow cell on screen 4");
        let mut g = untimed(two_at(1, 4, snow, 44, snow + 40));
        for f in 0..8 {
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.spies[0].terrain_t, f + 1);
            assert_eq!(g.spies[0].heat, 3, "eight passes is not yet a step");
        }
        assert_eq!(g.spies[0].terrain_t, 8, "the recorded counter reaches 8");
        frame2(&mut g, NOKEYS, NOKEYS);
        assert_eq!(g.spies[0].heat, 5, "the ninth pass is the step");
    }

    /// **The counter word itself is the period's witness.** Every dump carries `0x2F60`, and each
    /// branch zeroes it in the same pass that steps, so no sample can ever *show* a value at or
    /// above its own period. The highest value the corpus shows therefore pins each period from
    /// below: water 11 → the 12th pass steps; deep snow **8** → the 9th; the igloo floor 20 → the
    /// 21st. Nothing in this test is derived from the terrain model — only the code byte is, and it
    /// agrees with the original's own `0x2FAA` on 4,957 of 4,978 samples (the 21 are the projected
    /// cell of the last pass against the committed position, water margins only).
    #[test]
    fn the_recorded_terrain_counter_stays_under_its_period() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut seen = std::collections::BTreeMap::<u16, std::collections::BTreeSet<u16>>::new();
        for ses in ["freeze-death", "playtest1", "playtest2", "playtest4", "playtest5",
                    "trapulator-island4", "rocket-room-3-hard", "cold-check-1-5", "m3-heat"] {
            for path in dumps_of(ses).into_iter().take(400) {
                let Ok(st) = round::RoundState::from_ds_dump(&path) else { continue };
                let mut g = game(RoundOpts { island: 0, players: 2, ..RoundOpts::default() });
                g.levels = st.levels;
                for i in 0..2usize {
                    let p = &st.players[i];
                    if p.blank && i == 1 {
                        continue;
                    }
                    let Some(c) = g.terrain.code(&g.levels, p.band, p.spy.x as i32, p.spy.y as i32)
                    else { continue };
                    let t = u16::from_le_bytes([st.ds[0x2F60 + 2 * i], st.ds[0x2F61 + 2 * i]]);
                    seen.entry(c).or_default().insert(t);
                }
            }
        }
        let max = |c: u16| seen.get(&c).and_then(|v| v.iter().next_back().copied());
        eprintln!("0x2F60 by code: {seen:?}");
        assert_eq!(max(0x01), Some(11), "water: twelve passes a step");
        assert_eq!(max(0x02), Some(8), "deep snow: nine passes a step (`jg 8`)");
        assert_eq!(max(0x1D), Some(20), "the igloo floor: twenty-one passes a step");
        assert_eq!(max(0x03), Some(0), "ice keeps no counter");
        assert_eq!(max(0x05), Some(0), "a wall keeps no counter");
    }

    // ---- stage 22c: the clock-seeded placement (`CS:20cd`'s normal-play branch) --------------

    /// The four **position records** the report prints: the rocket parts (9, 10, 11) and the launch
    /// briefcase (12), as `(band, row, entry, code)` in placement order. `DS:0x3219/0x3205/0x320f`
    /// hold exactly these for codes 9..12, 272/272 checked against the recordings (STAGE22B §2).
    fn part_records(g: &Game) -> Vec<(u16, usize, usize, u8)> {
        g.layout
            .objects
            .iter()
            .filter(|o| (0x89..=0x8Cu8).contains(&o.code))
            .map(|o| (o.band, o.row, o.entry, o.code))
            .collect()
    }

    /// The whole object layer in placement order — the thing that must not repeat between rounds.
    fn object_signature(g: &Game) -> Vec<(u16, usize, usize, u8)> {
        g.layout.objects.iter().map(|o| (o.band, o.row, o.entry, o.code)).collect()
    }

    /// **The defect stage 22c fixes.** In the original the placement's `DX` is the DOS clock,
    /// re-read on every one of the ~260 draws (`CS:20cd`, `INT 21h AH=2Ch`), so each round's item
    /// layout is a fresh draw. The port pinned `DX` to the attract-mode `0x770D` *and* pinned the
    /// state to seed 0, so every round of every session had the same 14 objects and the same hatch.
    ///
    /// Fails against the stage-22b build: `RoundOpts` had no clock at all, so both of these rounds
    /// are the seed-0 layout.
    #[test]
    fn the_placement_can_be_taken_from_the_clock() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let at = |dh: u8, dl: u8| {
            game(RoundOpts { island: 0, players: 2, obj_clock: true, obj_clock_dx: Some((dh, dl)),
                             ..RoundOpts::default() })
        };
        let a = at(0x03, 0x37);
        let b = at(0x1D, 0x45);
        assert!(a.layout.objects.len() == 14 && b.layout.objects.len() == 14);
        assert_ne!(object_signature(&a), object_signature(&b),
                   "two different clock readings must give two different layouts");
        assert_ne!(part_records(&a), part_records(&b),
                   "the three parts + the briefcase move with the clock");
        // the same reading twice is the same layout: the clock is the only source of variation
        assert_eq!(object_signature(&at(0x03, 0x37)), object_signature(&a));
        // and neither of them is the frozen seed-0 layout the port used to draw every round
        let frozen = game(RoundOpts { island: 0, players: 2, ..RoundOpts::default() });
        assert_ne!(object_signature(&frozen), object_signature(&a));
        assert_ne!(object_signature(&frozen), object_signature(&b));
        eprintln!("stage 22c: clock 0337 {:?} hatch {:?}", part_records(&a), a.layout.hatch);
        eprintln!("stage 22c: clock 1d45 {:?} hatch {:?}", part_records(&b), b.layout.hatch);
    }

    /// **Every round takes its own reading of the clock, and a pinned round takes none.**
    ///
    /// `dos_clock_dx` counts its own calls, so the check is exact: a clock-seeded round samples the
    /// clock once (`CS:20cd`'s `int 21h ah=2ch`, modelled as one reading advanced by the draws — see
    /// `round::CLOCK_TICK_DRAWS`), a pinned `DX` round samples it zero times, and a second round is a
    /// second `Game::new` and therefore a second reading. That last point is the whole of the
    /// original's per-round variety: `CS:060d` re-enters `0x440` for every new round, and the port's
    /// GO! and `r` both build a fresh `Game` from `started_opts` (main.rs), so each one re-reads.
    #[test]
    fn every_round_takes_its_own_reading_of_the_clock() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        use std::sync::atomic::Ordering;
        let before = round::CLOCK_READS.load(Ordering::Relaxed);
        let _frozen = game(RoundOpts { island: 0, players: 2, ..RoundOpts::default() });
        let after_frozen = round::CLOCK_READS.load(Ordering::Relaxed);
        assert_eq!(after_frozen, before, "a pinned-DX round must not read the clock at all");

        let one = game(RoundOpts { island: 0, players: 2, obj_clock: true, ..RoundOpts::default() });
        let after_one = round::CLOCK_READS.load(Ordering::Relaxed);
        assert_eq!(after_one - after_frozen, 1, "one round = one reading");
        assert_eq!(one.layout.objects.len(), 14);
        assert!(one.layout.hatch.is_some(), "a clock round still finds a hatch");

        for _ in 0..3 {
            let g = game(RoundOpts { island: 0, players: 2, obj_clock: true, ..RoundOpts::default() });
            assert_eq!(g.layout.objects.len(), 14);
            assert!(g.layout.hatch.is_some(), "a clock round still finds a hatch");
        }
        let after_four = round::CLOCK_READS.load(Ordering::Relaxed);
        assert_eq!(after_four - after_one, 3, "each new round re-reads the clock once");
    }

    /// **A pinned layout is still a pinned layout.** `--seed N` (and `--obj-state`) must reproduce
    /// exactly what they did before stage 22c: the injected clock is inert unless `obj_clock` is
    /// set, and `obj_state` wins over the clock, so `--obj-state 0x469`/`0x0007` still replays the
    /// recorded attract round byte for byte.
    #[test]
    fn a_pinned_seed_or_state_still_ignores_the_clock() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let plain = game(RoundOpts { island: 0, players: 2, seed: 5, ..RoundOpts::default() });
        let noisy = game(RoundOpts { island: 0, players: 2, seed: 5, obj_clock_dx: Some((0x33, 0x77)),
                                     ..RoundOpts::default() });
        assert_eq!(object_signature(&plain), object_signature(&noisy),
                   "obj_clock_dx without obj_clock must change nothing");
        assert_eq!(plain.layout.hatch, noisy.layout.hatch);
        // and two constructions of the same seed agree — the replay contract
        let again = game(RoundOpts { island: 0, players: 2, seed: 5, ..RoundOpts::default() });
        assert_eq!(&plain.ds, &again.ds, "seed 5 must replay its own round byte for byte");

        // `--obj-state 0x469` is the attract round: the clock must be ignored even when it is on
        let attract = game(RoundOpts { island: 0, players: 2, obj_state: Some(0x0007),
                                       ..RoundOpts::default() });
        let attract_clock = game(RoundOpts { island: 0, players: 2, obj_state: Some(0x0007),
                                             obj_clock: true, obj_clock_dx: Some((0x51, 0x63)),
                                             ..RoundOpts::default() });
        assert_eq!(&attract.ds, &attract_clock.ds,
                   "obj_state must win over the clock, exactly as CS:20cd's attract branch does");
        assert_eq!(attract.layout.hatch, attract_clock.layout.hatch);
        assert_eq!(attract.fires, attract_clock.fires);
        assert_eq!(attract.layout.objects.len(), 14);
    }

    /// **The one-player divergence** (STAGE22B §1.2, edit B). `CS:257b` never reads `DS:0x31dd`: it
    /// places spy 2 (`si = 2`) and then spy 1 (`si = 0`) whatever the player count, and only the
    /// round's one-player word decides who drives spy 2 afterwards. The port used to skip spy 2's
    /// two draws in a one-player round, which shifted every later draw: at island 1 seed 5 that gave
    /// hatch band 4 entry 44 where the two-player round gives band 6 entry 83.
    #[test]
    fn a_one_player_round_places_the_same_layout_as_a_two_player_one() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for island in 0..7usize {
            for seed in 0..6u32 {
                let one = game(RoundOpts { island, players: 1, seed, ..RoundOpts::default() });
                let two = game(RoundOpts { island, players: 2, seed, ..RoundOpts::default() });
                assert_eq!(object_signature(&one), object_signature(&two),
                           "island {} seed {seed}: the object layer differs by player count",
                           island + 1);
                assert_eq!(one.layout.hatch, two.layout.hatch,
                           "island {} seed {seed}: the hatch differs by player count",
                           island + 1);
                assert_eq!(one.fires, two.fires,
                           "island {} seed {seed}: the fire lottery differs by player count",
                           island + 1);
                // spy 1's own start is the same draw in both rounds too
                assert_eq!((one.spies[0].screen, one.spies[0].x()), (two.spies[0].screen, two.spies[0].x()));
                // and the second spy is still blanked in the one-player round, with the record a
                // never-placed spy had before (screen 0, x 0) — the rendering is unchanged
                assert!(one.spies[1].blank, "island {} seed {seed}: spy 2 must be blank", island + 1);
                assert_eq!((one.spies[1].screen, one.spies[1].x(), one.spies[1].map_x), (0, 0, 0));
                assert!(!two.spies[1].blank);
            }
        }
        // the exact pair that seed gives, after stage 24a's placement fix: island 1, seed 5
        let one = game(RoundOpts { island: 0, players: 1, seed: 5, ..RoundOpts::default() });
        let two = game(RoundOpts { island: 0, players: 2, seed: 5, ..RoundOpts::default() });
        let h = one.layout.hatch.expect("a hatch");
        assert_eq!((h.band, h.entry), (4, 57),
                   "stage 24a: `CS:25FD`'s screen refusal costs one draw, not two — this seed's \
                    hatch moved from band 6 entry 83 (the old, one-draw-out stream) to band 4 \
                    entry 57, which is also the recorded attract round's hatch");
        assert_eq!(one.layout.hatch, two.layout.hatch);
    }

    /// **The numeric check for the report**: twelve different clock readings, the same island and
    /// nothing else pinned, and how many distinct part placements and hatches come out.
    ///
    /// The recorded corpus has **37 distinct layouts in 39 island-1 rounds** (STAGE22B §2); the
    /// port's 256-seed family has **26** distinct layouts and **4** hatch positions (§1.2). Twelve
    /// clock values must therefore beat the *pinned* family's variety per sample, and must not
    /// collapse onto the seed-0 layout.
    #[test]
    fn twelve_clock_readings_give_twelve_layouts() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let clocks = [(0x00u8, 0x0Du8), (0x03, 0x37), (0x07, 0x51), (0x0B, 0x19), (0x11, 0x63),
                      (0x17, 0x2F), (0x1D, 0x45), (0x1F, 0x77), (0x25, 0x1B), (0x29, 0x5D),
                      (0x2F, 0x31), (0x35, 0x69)];
        let mut layers: Vec<_> = Vec::new();
        let mut parts: Vec<_> = Vec::new();
        let mut hatches: Vec<(u16, usize, usize)> = Vec::new();
        for (dh, dl) in clocks {
            let g = game(RoundOpts { island: 0, players: 2, obj_clock: true,
                                     obj_clock_dx: Some((dh, dl)), ..RoundOpts::default() });
            assert_eq!(g.layout.objects.len(), 14, "clock {dh:02x}{dl:02x}: 14 objects");
            if !layers.contains(&object_signature(&g)) {
                layers.push(object_signature(&g));
            }
            if !parts.contains(&part_records(&g)) {
                parts.push(part_records(&g));
            }
            let h = g.layout.hatch.expect("a hatch");
            if !hatches.contains(&(h.band, h.row, h.entry)) {
                hatches.push((h.band, h.row, h.entry));
            }
        }
        eprintln!("stage 22c: 12 clock readings -> {} distinct object layers, {} distinct part                    placements, {} hatch positions {:?}", layers.len(), parts.len(), hatches.len(),
                  hatches);
        assert!(layers.len() >= 10, "only {} distinct layouts in 12 clock readings", layers.len());
        assert!(parts.len() >= 10, "only {} distinct part placements", parts.len());
        assert!(hatches.len() >= 2, "only {} hatch positions", hatches.len());
    }


    // ---------------------------------------------------------------------------------------
    // stage 23a — "a spy starts the round with an object in his hand"
    // ---------------------------------------------------------------------------------------

    /// One round's worth of the window's loop at the round's start: the GO! key is **Return**,
    /// which is also P1's fire key, and a human holds it for about a frame. `absorb` is what the
    /// caller does with the keys it saw when it created the round (main.rs's `play_app`).
    fn start_from_go(g: &mut Game, absorb: bool) {
        if absorb {
            g.absorb_held_keys(Input { p1: FIRE, p2: Keys::default() });
        }
        for t in 0..(FRAME_TICKS as usize * 3) {
            let keys = if t < FRAME_TICKS as usize { FIRE } else { Keys::default() };
            g.tick(Input { p1: keys, p2: Keys::default() });
        }
    }

    /// **The defect.** Island 2, clock `04:19`: the placement leaves the fuel canister (`0x8a`) on
    /// white's own start cell — the owner's "the White spy started with the Fuel tank IN HAND".
    /// The round's first frame then delivers the GO! press as a fire press, `FUN_26c0_2d22` calls
    /// `FUN_26c0_373b`, and the canister is in his hand before he has touched the keyboard.
    ///
    /// Fails against the stage-22c build: `absorbed` and `raw` are the same round and both end
    /// with `carried == 10`.
    #[test]
    fn the_go_press_does_not_take_an_item_off_the_floor() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let opts = RoundOpts { island: 1, players: 2, obj_clock: true,
                               obj_clock_dx: Some((0x04, 0x19)), ..RoundOpts::default() };
        let mut absorbed = game(opts.clone());
        let feet = absorbed.cell_of(0).expect("white's cell");
        assert_eq!(absorbed.ds_ref()[feet], 0x8a, "the placement must put the canister there");
        start_from_go(&mut absorbed, true);
        assert_eq!(absorbed.spies[0].carried, CARRY_NONE,
                   "the GO! key is not a fire press (carried {} = {})",
                   describe_carried(absorbed.spies[0].carried),
                   carried_name(absorbed.spies[0].carried));
        assert_eq!(absorbed.ds_ref()[feet], 0x8a, "and the canister is still on the floor");

        // the same round without the absorb: the press arrives twice and he picks it up on frame 0
        let mut raw = game(opts);
        start_from_go(&mut raw, false);
        assert_eq!(raw.spies[0].carried, 10, "the phantom press takes the canister");

        // and a real press, once the round is running, still takes it (`CS:2d22`)
        hold(&mut absorbed, FIRE, 1);
        hold(&mut absorbed, Keys::default(), FRAME_TICKS as usize);
        assert_eq!(absorbed.spies[0].carried, 10, "a fresh press takes it");
        assert_eq!(absorbed.ds_ref()[feet], 0, "and the cell is cleared");
    }

    /// **The rate.** 7 islands × 80 clock readings = 560 clock-seeded launches, each started the
    /// way the window starts one (Return held through the first frame). No spy may hold anything:
    /// the hand is written by `FUN_26c0_3xx` only, i.e. by a fire press.
    ///
    /// The sweep is not vacuous: it counts the layouts that put an item on a spy's own cell or on
    /// the entry in front of him — the ones the phantom press used to take (measured 70 of 560,
    /// and 32 of 560 actually taken on frame 0 before this fix).
    #[test]
    fn no_clock_seeded_round_starts_with_a_full_hand() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut on_item = 0;
        let mut launches = 0;
        for island in 0..7usize {
            for s in 0..80u32 {
                let dx = (((s * 7) % 60) as u8, ((s * 31 + 13) % 100) as u8);
                let mut g = game(RoundOpts { island, players: 2, obj_clock: true,
                                             obj_clock_dx: Some(dx), ..RoundOpts::default() });
                launches += 1;
                for i in 0..2 {
                    let feet = g.cell_of(i).map(|a| g.ds_ref()[a]).unwrap_or(0) & 0x1f;
                    let ahead = g.cell_ahead_of(i).map(|a| g.ds_ref()[a]).unwrap_or(0) & 0x1f;
                    if (1..=12).contains(&feet) || (1..=12).contains(&ahead) {
                        on_item += 1;
                    }
                }
                start_from_go(&mut g, true);
                for i in 0..2 {
                    assert_eq!(g.spies[i].carried, CARRY_NONE,
                               "island {} clock {:02x}{:02x}: spy {} starts holding {}",
                               island + 1, dx.0, dx.1, i,
                               carried_name(g.spies[i].carried));
                }
            }
        }
        eprintln!("stage 23a: {launches} clock-seeded round starts, 0 with a full hand; \
{on_item} of them put an item on a spy's own cell or in front of him");
        assert_eq!(launches, 560);
        assert!(on_item >= 30, "only {on_item} of 560 layouts put an item in reach");
    }

    // ---- stage 23c: the Big Storm (`CS:0B57..0x0BA2`) ---------------------------------------
    //
    // The original has no storm state machine: one signed word per spy (`DS:0x2F50`/+2) that
    // starts at `+1`, and inside each console's draw loop — while `DS:0x324D <= 120`, this spy is
    // the **host** of his own view and he is outdoors (`0x329F <= 8`) — six copies of the 410-byte
    // flake tile at `DS:0x5FEC` are OR'd into rows 0..55 of the view buffer, then the word is
    // negated. The sign picks which of two 16-px-apart copies of the tile is used, so the snow
    // twinkles every frame. Nothing else reads the word: no cold, no movement, no sound.
    // See `recon/round/port_check/STAGE23B_SPECS.md` section 1.

    /// `DS:0x5FEC` — the flake tile, exactly as the draw reads it (`0x19A` = 410 bytes).
    fn storm_tile() -> Vec<u8> {
        let img = exe();
        img[DS_FILE_OFF + 0x5FEC..DS_FILE_OFF + 0x5FEC + 0x19A].to_vec()
    }

    /// The tile as the draw reads it for a given phase copy: `si = 0x5FEC + phase`, 410 bytes.
    /// The `+4` copy drops the first two non-zero bytes of the tile (they fall off the front), which
    /// is why the two copies paint 52 and 50 bytes respectively on this image.
    fn storm_tile_at(phase: usize) -> Vec<u8> {
        let img = exe();
        img[DS_FILE_OFF + 0x5FEC + phase..DS_FILE_OFF + 0x5FEC + phase + 0x19A].to_vec()
    }

    /// What `CS:0B6C..0x0B9C` leaves in the console, done by hand: **six** copies of one tile, the
    /// destination walking 410 bytes straight through the buffer (never reset), the source
    /// re-based to `0x5FEC` each band and `phase` (0 or 4) bytes along.
    fn stormed(mut strip: [u8; STRIP_LEN], phase: usize) -> [u8; STRIP_LEN] {
        let img = exe();
        let tile = &img[DS_FILE_OFF + 0x5FEC + phase..DS_FILE_OFF + 0x5FEC + phase + 0x19A];
        for band in 0..6usize {
            for i in 0..0x19A {
                strip[band * 0x19A + i] |= tile[i];
            }
        }
        strip
    }

    /// The same console with the overlay suppressed (`Game::storm_off`) — the "before" of every
    /// before/after comparison in this section.
    fn calm_strip(g: &mut Game, player: usize) -> [u8; STRIP_LEN] {
        let saved = g.storm_off;
        g.storm_off = true;
        let s = g.strip_for(player, false);
        g.storm_off = saved;
        s
    }

    /// A one-spy round on island 1's outdoor screen 4, with the clock forced to `clock`.
    fn storm_game(clock: u16) -> Game {
        let mut g = game(RoundOpts {
            players: 1,
            start_screen: Some(4),
            start_y: Some(44),
            ..RoundOpts::default()
        });
        g.clock_left = clock;
        g.clock_word = 0;
        g
    }

    /// Painted-pixel difference between two 2bpp strips (4 pixels a byte).
    fn px_diff(a: &[u8; STRIP_LEN], b: &[u8; STRIP_LEN]) -> usize {
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| (0..4).filter(|k| ((x >> (2 * k)) & 3) != ((y >> (2 * k)) & 3)).count())
            .sum()
    }

    /// **The tile is the image's own bytes.** 52 non-zero bytes, 80 painted pixels a band (480 a
    /// console = 4.7 % of the viewport), every value a one- or two-pixel dash, no four-pixel run —
    /// which is why the storm reads as dots and not as a veil (`stage23b_mask.py --art`).
    #[test]
    fn the_storm_mask_is_the_images_own_bytes() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let t = storm_tile();
        assert_eq!(t.iter().filter(|&&b| b != 0).count(), 52, "52 non-zero bytes in the 410");
        assert_eq!(&t[..4], &[0x00, 0x3C, 0x00, 0xF0], "the tile's first four bytes");
        let mut seen: Vec<u8> = t.iter().copied().filter(|&b| b != 0).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, vec![0x03, 0x0F, 0x3C, 0xC0, 0xF0],
                   "every value is a 1-px (0x03/0xC0) or 2-px (0x0F/0x3C/0xF0) dash");
        let px: usize = t.iter().map(|&b| (0..4).filter(|k| (b >> (2 * k)) & 3 != 0).count()).sum();
        assert_eq!(px, 80, "80 painted pixels in a band");
        assert_eq!(px * 6, 480, "480 a console");
        // the 4-byte overhang the second copy reads is zeros, so nothing wraps
        let img = exe();
        assert!(img[DS_FILE_OFF + 0x5FEC + 0x19A..DS_FILE_OFF + 0x5FEC + 0x19E]
            .iter().all(|&b| b == 0));
    }

    /// **The gate is the clock and only the clock** (`CS:0B57`): at `0x324D = 121` the console is
    /// the stormless one, and the very frame the pod first reads `2:00` carries the overlay — at
    /// the `+1` copy, because the word is negated *after* the draw (`CS:0B9E`).
    #[test]
    fn the_storm_starts_on_the_frame_the_clock_reads_two_minutes() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = storm_game(122);
        while g.clock_left > 120 {
            let base = calm_strip(&mut g, 0);
            let now = g.strip_for(0, false);
            assert_eq!(now, base, "{} s: no storm yet", g.clock_left);
            assert_ne!(now, stormed(base, 4), "{} s: and no flakes either", g.clock_left);
            frame2(&mut g, NOKEYS, NOKEYS);
        }
        // now 120 or less — the frame the pod shows 2:00
        let base = calm_strip(&mut g, 0);
        let storm = g.strip_for(0, false);
        assert_eq!(storm, stormed(base, 4),
                   "the 2:00 frame is the first with the overlay, at the +1 copy");
        assert_ne!(storm, base);
        let painted = px_diff(&storm, &base);
        assert!(painted > 0 && painted <= 480,
                "the overlay paints at most 480 pixels, got {painted}");
    }

    /// **Six copies of one tile, 410 bytes apart** — 410 is 9 rows and 14 bytes of the 44-byte
    /// strip, so each stamp lands 14 bytes further right than the last: the tile walks a diagonal,
    /// it is not a row-aligned tiling. Every masked byte must be present at all six bands.
    #[test]
    fn the_storm_is_six_copies_of_one_tile_on_a_diagonal() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        assert_eq!(0x19A % STRIP_PITCH, 14, "410 bytes is 9 rows + 14 bytes of a 44-byte strip");
        assert_eq!(6 * 0x19A, 2460, "bands 0..5 cover bytes 0..2459 — rows 0..55 of the 58-row view");
        let mut g = storm_game(100);
        let s = g.strip_for(0, false);
        let base = calm_strip(&mut g, 0);
        // a fresh round draws the `+1` copy first, i.e. the tile read 4 bytes along
        let t = storm_tile_at(4);
        let per_band = t.iter().filter(|&&b| b != 0).count();
        assert_eq!(per_band, 50, "the +1 copy loses the two non-zero bytes at the tile's front");
        let mut masked = 0usize;
        for band in 0..6usize {
            for i in 0..0x19A {
                if t[i] == 0 {
                    continue;
                }
                masked += 1;
                assert_eq!(s[band * 0x19A + i] & t[i], t[i],
                           "band {band} byte {i}: the tile must be OR'd in here");
            }
        }
        assert_eq!(masked, per_band * 6, "six stamps of one 410-byte tile");
        // and the diagonal itself: the tile's byte 0 lands at buffer byte 410, row 9 column 14
        assert_eq!(s[410], base[410] | t[0]);
        assert_eq!(s[410 + 1], base[411] | t[1]);
    }

    /// **The word flips and the mask follows it** (`CS:0B7D`/`0x0B9E`): the phase is `-1` or `+1`,
    /// the sign picks the copy (4 bytes along for non-negative) and it negates once per drawn
    /// frame, so consecutive storm frames alternate between the two copies 16 px apart.
    #[test]
    fn the_storm_phase_alternates_and_the_mask_follows_it() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = storm_game(100);
        let mut seen = Vec::new();
        for f in 0..4 {
            // the console is read *after* the frame, which is where the renderer sits: the
            // original draws with W and stores -W, so frame N shows the pre-flip word
            frame2(&mut g, NOKEYS, NOKEYS);
            let base = calm_strip(&mut g, 0);
            let s = g.strip_for(0, false);
            let want = if f % 2 == 0 { 4 } else { 0 };
            assert_eq!(s, stormed(base, want), "frame {f}: phase copy {want}");
            seen.push(want);
        }
        assert_eq!(seen, vec![4, 0, 4, 0]);
    }

    /// **No storm indoors, on a visitor's console, or with no host** — `CS:0B5E` (`0x2F7E`) and
    /// `CS:0B65` (`0x329F <= 8`). Measured on the corpus: 12 storm-eligible dumps on screen 14/16
    /// carry no flakes; the outdoor dumps carry all 300 masked bytes.
    #[test]
    fn the_storm_skips_indoors_and_the_visitor_console() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // indoors: an igloo interior is screen >= 10
        let mut indoor = storm_game(100);
        indoor.spies[0].screen = 14;
        let base = calm_strip(&mut indoor, 0);
        let s = indoor.strip_for(0, false);
        assert_eq!(s, base, "an igloo interior gets no snow");
        // every outdoor id is eligible: 0, 2, 4, 6, 8
        for screen in [0u16, 2, 4, 6, 8] {
            let mut g = storm_game(100);
            g.spies[0].screen = screen;
            g.spies[0].map_x = 0;
            let base = calm_strip(&mut g, 0);
            let s = g.strip_for(0, false);
            assert_eq!(s, stormed(base, 4), "screen {screen} is outdoors and snows");
        }
        // the visitor's console: `0x2F7E` set -> flat cyan, no flakes
        let mut visitor = storm_game(100);
        visitor.spies[0].blank = true;
        let s = visitor.strip_for(0, false);
        assert!(s.iter().all(|&b| b == 0x55), "the visitor console stays the flat plate");
        // no host (both visitor flags set): each composes his own room, and neither snows
        let mut no_host = game(RoundOpts {
            players: 2,
            start_screen: Some(4),
            start_y: Some(44),
            start_x2: Some(120),
            ..RoundOpts::default()
        });
        no_host.clock_left = 100;
        no_host.spies[0].blank = true;
        no_host.spies[1].blank = true;
        for p in 0..2usize {
            let base = calm_strip(&mut no_host, p);
            let s = no_host.strip_for(p, false);
            assert_eq!(s, base, "console {p}: no host, no snow");
        }
    }

    /// **The storm is a picture, not a rule.** Sixty frames of walking in the storm leave heat,
    /// both cold counters, the sink, the position and the speaker output identical to sixty frames
    /// with the gate shut — `storm-expiry` measured 5.45/5.85 px/s calm against 5.58/5.71 in the
    /// storm, i.e. the manual's "the storm slows you down" is not in this build.
    #[test]
    fn the_storm_does_not_touch_heat_movement_or_sound() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut a = storm_game(120);
        let mut b = storm_game(120);
        b.storm_off = true;
        for _ in 0..60 {
            frame2(&mut a, RIGHT, NOKEYS);
            frame2(&mut b, RIGHT, NOKEYS);
        }
        assert!(b.spies[0].x() != 0 || b.spies[0].map_x != 0, "somebody moved");
        assert!(a.clock_left < 121, "the storm's game still has the real clock");
        for (what, x, y) in [
            ("heat", a.spies[0].heat as i64, b.spies[0].heat as i64),
            ("base_t", a.spies[0].base_t as i64, b.spies[0].base_t as i64),
            ("terrain_t", a.spies[0].terrain_t as i64, b.spies[0].terrain_t as i64),
            ("pose_clip", a.spies[0].pose_clip as i64, b.spies[0].pose_clip as i64),
            ("x3", a.spies[0].x3 as i64, b.spies[0].x3 as i64),
            ("y3", a.spies[0].y3 as i64, b.spies[0].y3 as i64),
            ("frame", a.spies[0].frame as i64, b.spies[0].frame as i64),
        ] {
            assert_eq!(x, y, "{what} must not depend on the storm");
        }
        assert_eq!(format!("{:?}", a.sound_out), format!("{:?}", b.sound_out),
                   "the storm makes no sound");
        // and the storm really was drawing on all 60 of them: the console matches one of the two
        // copies of the tile, which a stormless one cannot
        let base = calm_strip(&mut a, 0);
        let s = a.strip_for(0, false);
        assert!(s == stormed(base, 4) || s == stormed(base, 0), "still snowing after 60 frames");
    }

    // ---- stage 23c: the shove's second terrain dispatch (`CS:0A97..0x0AFD`) -----------------
    //
    // The tail of the move loop shoves the other spy one unit and then runs **his** terrain
    // dispatch a second time that frame: `CS:0A9B` moves him, `0x0AAB` clears `0x31AA`, `0x0AB1`
    // probes the cell, `0x0AB4` calls `FUN_26c0_1e0f`, and `0x0AB9` undoes the move **if and only
    // if** the dispatch ran `FUN_26c0_3396`. The sink and the cold counter are written before that
    // arm can be reached, so they survive the undo. Evidence level: **read** (no recorded frame
    // isolates a shove into water or snow — see the report).

    /// Two spies nine units apart on one screen, the east one the **visitor** (blank), so
    /// `push_in_step`'s `0x2F7E` gate is open. Both press RIGHT.
    fn two_shoving_east(host_x: i32, other_x: i32) -> Game {
        let mut g = two_on_one_screen(host_x, other_x, (host_x - 20).clamp(0, MAP_X_MAX));
        // the shove's gate is "somebody is on the other's board" — either flag will do
        g.spies[1].blank = true;
        g
    }

    /// **The shoved spy runs his own dispatch too** — one more tick of the terrain counter than his
    /// own frame gave him, from the shove's pass.
    #[test]
    fn a_shoved_spy_runs_his_terrain_dispatch_too() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_shoving_east(140, 150);
        g.spies[1].screen = 4;
        g.spies[1].y3 = 44 * SUB;
        let (x, t, sink) = (g.spies[1].x(), g.spies[1].terrain_t, g.spies[1].pose_clip);
        g.push_in_step(0, [RIGHT, RIGHT]);
        assert_eq!(g.spies[1].x(), x + 1, "the shove moved him one unit");
        assert_eq!(g.spies[1].terrain_t, t, "open ground dispatches nothing (the counter is 0)");
        assert_eq!(g.spies[1].pose_clip, sink);

        // and on deep snow the shove's own pass shows
        let mut w = deep_snow_walker();
        w.players = 2;
        w.spies[1].screen = 4;
        w.spies[1].x3 = w.spies[0].x3 + 9 * SUB;
        w.spies[1].y3 = 44 * SUB;
        w.spies[1].blank = true;
        let t1 = w.spies[1].terrain_t;
        let clip1 = w.spies[1].pose_clip;
        w.push_in_step(0, [RIGHT, RIGHT]);
        assert_eq!(w.spies[1].terrain_t, t1 + 1, "the shove dispatched him a second time");
        assert_eq!(w.spies[1].pose_clip, clip1 + 2, "and the sink came with it");
    }

    /// **A shove the dispatch refuses is undone — but only the x.** With the trudge in, an odd roll
    /// in the shoved cell is a `3396`: he ends the frame where he started, still sunk and still
    /// colder, which is what `CS:0AB9` literally says.
    #[test]
    fn a_shove_that_the_dispatch_refuses_is_undone_but_the_sink_stays() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = deep_snow_walker();
        g.players = 2;
        // the victim stands one unit west of the snow, so the shove pushes him **onto** it
        let snow = g.spies[0].x();
        g.spies[0].x3 = (snow - 6) * SUB;
        g.spies[1].screen = 4;
        g.spies[1].x3 = (snow - 1) * SUB;
        g.spies[1].y3 = 44 * SUB;
        g.spies[1].blank = true;
        g.stand_roll = Some(0x05); // odd -> the trudge stops him
        let (x, clip, t) = (g.spies[1].x(), g.spies[1].pose_clip, g.spies[1].terrain_t);
        g.push_in_step(0, [RIGHT, RIGHT]);
        assert_eq!(g.spies[1].x(), x, "the shove was undone: `0x31AA` was set");
        assert_eq!(g.spies[1].pose_clip, clip + 2, "but the sink survives the undo");
        assert_eq!(g.spies[1].terrain_t, t + 1, "and so does the counter");
        assert!(g.spies[1].refused);
    }

    /// **The control**: a shove onto open ground is kept, and the victim pays nothing for it.
    #[test]
    fn a_shove_onto_open_ground_is_kept() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_shoving_east(140, 150);
        let (x, clip, t) = (g.spies[1].x(), g.spies[1].pose_clip, g.spies[1].terrain_t);
        assert_eq!(g.terrain.code(&g.levels, 4, x + 1, 44), Some(terrain::T_OPEN));
        g.push_in_step(0, [RIGHT, RIGHT]);
        assert_eq!(g.spies[1].x(), x + 1);
        assert_eq!(g.spies[1].pose_clip, clip);
        assert_eq!(g.spies[1].terrain_t, t);
        assert!(!g.spies[1].refused, "nothing refused it");
    }

    /// **A shove at a wall is refused by the dispatch, not by a solid-cell test** — the spy does not
    /// move and the wall's own `3396` is what says so (`0x31AA`).
    #[test]
    fn a_shove_at_a_wall_is_refused_by_the_dispatch() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = two_shoving_east(140, 150);
        // put the victim against the igloo's solid body on island 1 screen 4 (level 16, column 6)
        let wall = (0..=300)
            .find(|&x| g.terrain.code(&g.levels, 4, x, 44) == Some(terrain::T_SOLID))
            .expect("a solid cell on screen 4");
        g.spies[1].x3 = (wall - 1) * SUB;
        g.spies[1].y3 = 44 * SUB;
        let x = g.spies[1].x();
        g.push_in_step(0, [RIGHT, RIGHT]);
        assert_eq!(g.spies[1].x(), x, "he did not enter the wall");
        assert!(g.spies[1].refused, "and it was the dispatch that refused, `CS:0AB9`");
    }

    // ---- stage 23c: deep snow's trudge (`CS:1E8B..0x1EBD`) --------------------------------
    //
    // `CS:1e95` calls the time-seeded PRNG and, on an **odd** low byte, calls `FUN_26c0_3396` — the
    // stop — so that pass commits nothing. The sink and the cold counter sit *after* the branch and
    // run either way, and the snowshoes in hand skip the whole arm before the roll (`CS:1e92`).
    // Nothing else in deep snow can stop a spy, so a single-unit frame while walking it is the
    // trudge's own signature (measured: `playtest2`, 2 of 9 moving frames at 1 unit).

    /// A one-spy round standing on deep snow on island 2's screen 4, walking east.
    fn deep_snow_walker() -> Game {
        let probe = game(RoundOpts { island: 1, start_screen: Some(4), ..RoundOpts::default() });
        let snow = x_on_code(&probe, 4, terrain::T_DEEP_SNOW, 44)
            .expect("island 2 has a deep-snow cell on screen 4");
        let mut g = game(RoundOpts {
            island: 1,
            players: 1,
            start_screen: Some(4),
            start_x: Some(snow),
            start_y: Some(44),
            ..RoundOpts::default()
        });
        assert_eq!(g.terrain.code(&g.levels, 4, snow, 44), Some(terrain::T_DEEP_SNOW));
        g.spies[0].map_x = (snow - MAP_X_ON_ENTRY_OFFSET).clamp(0, MAP_X_MAX);
        g
    }

    /// **An odd roll stops that pass** (`CS:1e95`..`0x1e9d`): even, the spy walks; odd, `3396`
    /// zeroes both velocities and the legs go back to the standing frame, so `CS:0a1a` commits the
    /// position he was already on.
    #[test]
    fn deep_snow_stops_the_spy_on_an_odd_random_byte() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = deep_snow_walker();
        g.stand_roll = Some(0x02); // even
        let x0 = g.spies[0].x();
        frame2(&mut g, RIGHT, NOKEYS);
        assert!(g.spies[0].x() > x0, "an even byte leaves the pass alone");

        g.stand_roll = Some(0x03); // odd
        let x1 = g.spies[0].x();
        for _ in 0..3 {
            frame2(&mut g, RIGHT, NOKEYS);
        }
        assert_eq!(g.spies[0].x(), x1, "an odd byte stops every pass");
        assert_eq!(g.spies[0].frame, STAND_FRAME, "`3396` puts the legs on the standing frame");
        assert_eq!(g.spies[0].frame_step, 0);
    }

    /// **The trudge stops the step, not the terrain** — the sink (`+2`, clamped at 6) and the cold
    /// counter sit after the branch (`CS:1ea0`/`0x1eb2`), so a stopped pass still pays for the
    /// snow it is standing in.
    #[test]
    fn the_trudge_does_not_stop_the_cold_or_the_sink() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = deep_snow_walker();
        g.stand_roll = Some(0x01); // odd
        let (clip, t, x) = (g.spies[0].pose_clip, g.spies[0].terrain_t, g.spies[0].x());
        frame2(&mut g, RIGHT, NOKEYS);
        assert_eq!(g.spies[0].x(), x, "the pass was stopped");
        assert_eq!(g.spies[0].pose_clip, clip + 2, "the sink still ran");
        assert_eq!(g.spies[0].terrain_t, t + 1, "and the cold counter");
    }

    /// **The snowshoes skip the roll itself** — `CS:1e92` jumps past the whole arm, so a spy
    /// carrying them (code 7) consumes nothing from the trudge's stream and pays no sink.
    #[test]
    fn the_snowshoes_skip_the_trudge_too() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = deep_snow_walker();
        g.spies[0].carried = 7;
        let before = g.stand_rng.state();
        frame2(&mut g, RIGHT, NOKEYS);
        assert_eq!(g.stand_rng.state(), before, "no roll is taken with the snowshoes in hand");
        assert_eq!(g.spies[0].pose_clip, 0, "and no sink");
        assert_eq!(g.spies[0].terrain_t, 0, "and no counter");
        assert!(g.spies[0].x() > 0);
    }

    /// **Half the passes, over a long walk**: 200 passes on a free stream stop in 90..110 of them.
    #[test]
    fn deep_snow_halves_the_distance_over_a_long_walk() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = deep_snow_walker();
        let mut stopped = 0;
        for _ in 0..200 {
            g.spies[0].vel_x = WALK_X_PER_FRAME;
            g.terrain_tick(0);
            if g.spies[0].vel_x == 0 {
                stopped += 1;
            }
        }
        assert!((90..=110).contains(&stopped), "{stopped} of 200 passes stopped");
    }

    /// **The recorded storm consoles.** The spec's list (`STAGE23B_SPECS.md` §1.9): dumps that are
    /// byte-exact against the mask at one of the two copies, from the two sessions that reach 2:00.
    /// For each, the port composes the console from the dump's own state and must equal the
    /// recorded `DS:0x1A92` — at the dumped phase word or its negation (the dump is taken after the
    /// flip, so the word it holds is the negation of what that frame drew).
    #[test]
    fn every_recorded_storm_strip() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let list: [(&str, &[&str]); 2] = [
            ("storm-expiry", &["ds_00165.bin", "ds_00167.bin", "ds_00168.bin", "ds_00170.bin",
                               "ds_00172.bin", "ds_00175.bin", "ds_00178.bin", "ds_00181.bin",
                               "ds_00184.bin", "ds_00186.bin", "ds_00188.bin", "ds_00193.bin",
                               "ds_00196.bin", "ds_00198.bin", "ds_00202.bin", "ds_00204.bin"]),
            ("playtest", &["ds_00553.bin", "ds_00554.bin", "ds_00555.bin", "ds_00557.bin",
                           "ds_00560.bin", "ds_00562.bin", "ds_00565.bin", "ds_00570.bin",
                           "ds_00575.bin", "ds_00578.bin"]),
        ];
        let mut tried = 0usize;
        let mut exact = 0usize;
        let mut at_dumped_word = 0usize;
        let mut missing = 0usize;
        let mut first_bad: Vec<String> = Vec::new();
        for (ses, names) in list {
            for name in names {
                let p = crate::orig::require("recon/round/sessions").join(ses).join(name);
                if !p.exists() {
                    missing += 1;
                    continue;
                }
                let st = round::RoundState::from_ds_dump(&p).expect("dump");
                let want = &st.ds[round::DS_STRIP..round::DS_STRIP + STRIP_LEN];
                let word = u16::from_le_bytes([st.ds[0x2F50], st.ds[0x2F51]]) as i16;
                tried += 1;
                let mut best = usize::MAX;
                let mut matched_at_word = false;
                for (k, sign) in [1i16, -1].into_iter().enumerate() {
                    let mut g = game(RoundOpts { players: 2, ..RoundOpts::default() });
                    g.seed_from_dump(&st, true);
                    g.spies[0].storm_phase = if word != 0 { sign * word.abs() } else { sign };
                    let got = g.engine_strip(0);
                    let d = got.iter().zip(want).filter(|(a, b)| a != b).count();
                    if d < best {
                        best = d;
                    }
                    if d == 0 {
                        exact += 1;
                        if k == 0 {
                            at_dumped_word += 1;
                            matched_at_word = true;
                        }
                        break;
                    }
                }
                if best != 0 && first_bad.len() < 6 {
                    first_bad.push(format!("{ses}/{name}: {best} bytes differ"));
                }
                let _ = matched_at_word;
            }
        }
        eprintln!("stage 23c: storm strips {exact} of {tried} byte-exact \
({at_dumped_word} at the dumped word, {missing} dumps missing); misses: {first_bad:?}");
        assert_eq!(missing, 0, "the corpus must be present");
        assert!(exact >= 20, "at least 20 of the 26 listed dumps must compose exactly, got {exact}");
    }

    /// **The map window is drawn over the snow** (`CS:0BA3` comes after the OR): every byte the
    /// open map writes must hold the map's value, not the flakes'.
    #[test]
    fn the_map_is_drawn_over_the_snow() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = storm_game(100);
        g.spies[0].map_open = true;
        let s = g.strip_for(0, false);
        let m = calm_strip(&mut g, 0);
        let mut shut = storm_game(100);
        shut.spies[0].map_open = false;
        let n = calm_strip(&mut shut, 0);
        let mut written = 0usize;
        for i in 0..STRIP_LEN {
            if m[i] != n[i] {
                written += 1;
                assert_eq!(s[i], m[i], "byte {i}: the map wins over the overlay");
            }
        }
        assert!(written > 100, "the map wrote {written} bytes");
        assert_ne!(s, m, "and the storm is still on everywhere the map is not");
    }

    // ---- stage 24a: the attract demo ----------------------------------------------------------

    /// **A demo round is both spies on the recorded stream, and nothing else.**
    ///
    /// `CS:0325`'s attract branch calls `CS:0203` and returns *before* the keyboard copy and before
    /// the `CS:129C` call, so the frame's six flags per spy come from `DEMO.DAT`'s mask alone. The
    /// mask here is hand-made: spy 1 walks right (bit 1), spy 2 walks left (bit 11) — 2 units a
    /// frame while the view scrolls, as a walking spy does.
    #[test]
    fn a_demo_round_drives_both_spies_from_the_recorded_masks() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let masks = vec![0u16; 4];
        let mut g = game(RoundOpts {
            players: 2,
            demo: Some(masks),
            ..RoundOpts::default()
        });
        assert!(g.is_demo());
        assert_eq!(g.players, 2, "a demo always has both spies in it");
        assert!(g.cpu.iter().all(|c| c.is_none()), "and neither of them is the computer");
        assert_eq!(g.demo_frames(), 4);
        let (x1, x2) = (g.spies[0].x(), g.spies[1].x());
        // frame 0: no keys at all, both stand.
        hold(&mut g, Keys::default(), FRAME_TICKS as usize);
        assert_eq!((g.spies[0].x(), g.spies[1].x()), (x1, x2));
        // frame 1: spy 1 right, spy 2 left.
        g.demo = Some(vec![0u16, 0x0002 | 0x0800]);
        hold(&mut g, Keys::default(), FRAME_TICKS as usize);
        assert!(g.spies[0].x() > x1, "spy 1 walked right: {} -> {}", x1, g.spies[0].x());
        assert!(g.spies[1].x() < x2, "spy 2 walked left: {} -> {}", x2, g.spies[1].x());
    }

    /// **The demo does not pick up the keyboard** — the owner's "GO!-held keys must not leak".
    ///
    /// The panel's timeout fires with no key down, but a demo can also be entered while a key is
    /// held (the port's `--demo` runs alongside a script), and the original's `CS:0203` clears all
    /// twelve flags and re-sets them from the mask, so the keyboard is invisible. Feeding both runs
    /// the same keys *and* different keys must give byte-identical state.
    #[test]
    fn the_demo_ignores_the_keyboard_entirely() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let masks: Vec<u16> = (0..40).map(|i| if i % 7 == 0 { 0x0002 } else { 0 }).collect();
        let run = |keys: Keys| -> Vec<(u16, i32, i32, i32, i32, u16)> {
            let mut g = game(RoundOpts {
                players: 2,
                demo: Some(masks.clone()),
                ..RoundOpts::default()
            });
            let mut out = Vec::new();
            for _ in 0..40 {
                for _ in 0..FRAME_TICKS {
                    g.tick(Input { p1: keys, p2: keys });
                }
                out.push((
                    g.frames as u16,
                    g.spies[0].x(), g.spies[0].y(),
                    g.spies[1].x(), g.spies[1].y(),
                    g.mask,
                ));
            }
            out
        };
        let still = run(Keys::default());
        let mashed = run(Keys { up: true, down: true, left: true, right: true,
                                fire: true, fire_b: true });
        assert_eq!(still, mashed, "the keyboard cannot reach a demo frame");
        assert!(still.iter().any(|r| r.1 != still[0].1), "and the stream did move spy 1");
    }

    /// **The demo is deterministic** — same masks, same round, same frames; and it is a *pure*
    /// function of the stream, so two runs differ the moment one mask differs.
    #[test]
    fn the_demo_is_deterministic() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let masks: Vec<u16> = (0..30).map(|i| [0x0002u16, 0x0004, 0x0001][i % 3]).collect();
        let snap = || {
            let mut g = game(RoundOpts {
                players: 2,
                demo: Some(masks.clone()),
                ..RoundOpts::default()
            });
            let mut v = Vec::new();
            for _ in 0..30 {
                for _ in 0..FRAME_TICKS { g.tick(Input::default()); }
                v.push((g.spies[0].x(), g.spies[0].y(), g.spies[1].x(), g.spies[1].y(),
                        g.spies[0].heat, g.spies[1].heat, g.clock_left));
            }
            v
        };
        assert_eq!(snap(), snap(), "two runs of one stream are identical");
        let mut other = masks.clone();
        other[10] = 0x0008;
        let mut g = game(RoundOpts { players: 2, demo: Some(other), ..RoundOpts::default() });
        for _ in 0..11 * FRAME_TICKS { g.tick(Input::default()); }
        let mut g2 = game(RoundOpts { players: 2, demo: Some(masks), ..RoundOpts::default() });
        for _ in 0..11 * FRAME_TICKS { g2.tick(Input::default()); }
        assert_ne!((g.spies[0].x(), g.spies[0].y()), (g2.spies[0].x(), g2.spies[0].y()),
                   "one changed mask changes the walk");
    }

    /// **Past the end of the stream nothing is pressed**, and the demo keeps running: `CS:0203`
    /// never rewinds `DS:0x0FB6` and nothing bounds it, so it reads whatever the load left above
    /// `DS:0x000D + 1882` — zero in both recorded demos (`m1-fix-960`, `playtest3`).
    #[test]
    fn a_demo_past_its_stream_holds_no_keys() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(RoundOpts {
            players: 2,
            demo: Some(vec![0x0002, 0x0002]), // two frames of "spy 1 right"
            ..RoundOpts::default()
        });
        assert_eq!(g.demo_mask(0), 0x0002);
        assert_eq!(g.demo_mask(1), 0x0002);
        assert_eq!(g.demo_mask(2), 0, "past the stream: nothing pressed");
        assert_eq!(g.demo_mask(900), 0);
        for _ in 0..6 * FRAME_TICKS { g.tick(Input::default()); }
        let stopped = (g.spies[0].x(), g.spies[0].y());
        for _ in 0..6 * FRAME_TICKS { g.tick(Input::default()); }
        assert_eq!((g.spies[0].x(), g.spies[0].y()), stopped, "and the spy comes to rest");
        assert_eq!(g.phase, Phase::Playing, "the round runs on: the clock ends it");
    }

    /// **The attract round's own placement, against the recording.**
    ///
    /// `--obj-state 0x469` with `DX = 0x770D` is the attract round; the recorded demo dumps
    /// (`m1-fix-960` ds_00003, `playtest3` ds_00246, identical) read spy 1 at screen 6 x 112,
    /// spy 2 at screen 4 x 137, the hatch at band 4 row 3 entry 57, and fourteen objects. The spy
    /// draws are the tail of `CS:257B`, and `CS:25FD`'s "player 1 refuses player 2's screen" costs
    /// **one** draw (it jumps back to the screen draw at `0x25F2`), not two — stage 24a fixed the
    /// port to match, which is what puts spy 1 on x 112 (it was 179) and the hatch on band 4.
    #[test]
    fn the_attract_round_places_both_spies_and_the_hatch_where_the_recording_does() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = game(RoundOpts {
            players: 2,
            obj_state: Some(round::RNG_ATTRACT_SEED),
            launch: true,
            ..RoundOpts::default()
        });
        assert_eq!((g.spies[0].screen, g.spies[0].x(), g.spies[0].y()), (6, 112, 44));
        assert_eq!((g.spies[1].screen, g.spies[1].x(), g.spies[1].y()), (4, 137, 44));
        let h = g.layout.hatch.expect("the attract round has a hatch");
        assert_eq!((h.band, h.row, h.entry), (4, 3, 57), "the recorded hatch");
        assert_eq!(g.layout.objects.len(), 14, "the recorded object count");
    }

    // ---- stage 24b: the shared generator and the replay's own walk --------------------------

    /// **Stage 24b — the shared generator is left where the recording leaves it.**
    ///
    /// `DS:0x31B0` is the word `CS:20CD` keeps its state in, and it is the one number that can
    /// check the placement's *draw order* against the recording without a position in the way. The
    /// attract round's first recorded dump holds **`0x090A`** (`m1-fix-960` ds_00003, `playtest3`
    /// ds_00246) and all 244 demo dumps of both sessions hold it unchanged, so the placement's
    /// draws are the whole story and the round itself draws nothing.
    ///
    /// The draw count is this build's own measurement for the attract round (**275** draws: island
    /// 1's placement, the hatch, and the igloo-fire pass, all off one stream; STAGE22B §2.2's 261
    /// was a one-player clock-seeded round, which takes a different route through the two re-roll
    /// loops). A single extra or missing draw moves the word; that is exactly the defect stage 24a
    /// found in `CS:25FD` (a screen refusal that cost two draws instead of one), and it is why the
    /// two numbers are asserted together.
    #[test]
    fn the_attract_round_leaves_the_shared_generator_where_the_recording_does() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = game(RoundOpts {
            players: 2,
            obj_state: Some(round::RNG_ATTRACT_SEED),
            launch: true,
            ..RoundOpts::default()
        });
        assert_eq!(
            g.placed_rng_state, 0x090A,
            "DS:0x31B0 after the attract placement (every recorded demo dump)"
        );
        assert_eq!(g.placed_rng_draws, PLACEMENT_DRAWS, "the placement's draw count");
    }

    /// How many draws the attract placement spends. Measured, and pinned by
    /// `the_attract_round_leaves_the_shared_generator_where_the_recording_does`: the count and the
    /// resulting `0x31B0` are checked together, so a stream that reached the right word by a
    /// different route would still fail.
    const PLACEMENT_DRAWS: u32 = 275;

    /// **Stage 24b — the replay's own walk, at the recorded frames.**
    ///
    /// Every number below is read out of `m1-fix-960`'s demo dumps (`DS:0x329F/0x329B/0x3297` for
    /// spy 1, `0x32A1/0x329D/0x3299` for spy 2, `0x2F58`/`0x2F5A` heat, `0x2F2C` in hand,
    /// `0x32A3` kit) at the dump whose `0x0FB6/2` equals the frame:
    ///
    /// | frame | the recording |
    /// |---|---|
    /// | 1 | spy 1 (6,112,44) heat 3, kit `0x83A`; spy 2 (4,137,44) heat 3 |
    /// | 44 | spy 1 (6,112,56) taking item 3, kit `0x838` |
    /// | 72 | spy 1 (6,105,56) with nothing in hand, kit `0x83A`; spy 2 (6,126,54) |
    /// | 175 | spy 1 (6,108,56) heat 35; spy 2 (6,126,54) heat 31 |
    ///
    /// These are the frames the port and the recording agree on exactly, and they cover every
    /// mechanism the demo's first half exercises: the stored masks, the trapulator take and
    /// put-away, both walking spasms of the snowball duel, the heat drains and the two consoles'
    /// screens. `STAGE24B_REPORT.md` §2 has the full-length table — the first divergence of a
    /// position is at frame 180, so everything up to 179 is asserted here.
    #[test]
    fn the_demo_replays_the_recorded_walk_frame_by_frame() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let p = crate::orig::require("spy-vs-spy-iii/original/256/DEMO.DAT");
        let Ok(bytes) = std::fs::read(&p) else { return };
        let stream = round::demo_masks(&bytes);
        let mut g = game(RoundOpts {
            players: 2,
            obj_state: Some(round::RNG_ATTRACT_SEED),
            launch: true,
            demo: Some(stream),
            ..RoundOpts::default()
        });
        let pos = |g: &Game, i: usize| (g.spies[i].screen, g.spies[i].x(), g.spies[i].y());
        let step_to = |g: &mut Game, n: u64| {
            while g.frames < n {
                for _ in 0..FRAME_TICKS {
                    g.tick(Input::default());
                }
            }
        };

        step_to(&mut g, 1);
        assert_eq!(pos(&g, 0), (6, 112, 44), "the attract round's spy 1");
        assert_eq!(pos(&g, 1), (4, 137, 44), "and spy 2, on his own console");
        assert_eq!((g.spies[0].heat, g.spies[1].heat), (3, 3));
        assert_eq!(g.spies[0].kit, 0x83A, "the kit word the dump holds at frame 1");

        step_to(&mut g, 44);
        assert_eq!(g.spies[0].carried, 3, "spy 1 takes the water bucket at frame 44");
        assert_eq!(g.spies[0].kit, 0x838, "the take clears the tool's kit bit");
        assert_eq!(pos(&g, 0), (6, 112, 56));

        step_to(&mut g, 72);
        assert_eq!(g.spies[0].carried, CARRY_NONE, "and puts it away at frame 72");
        assert_eq!(g.spies[0].kit, 0x83A);
        assert_eq!(pos(&g, 0), (6, 105, 56));
        assert_eq!(pos(&g, 1), (6, 126, 54), "spy 2 has come down into spy 1's room");

        step_to(&mut g, 174);
        assert_eq!(pos(&g, 0), (6, 108, 56));
        assert_eq!(pos(&g, 1), (6, 126, 54));
        // `demo-full-24c` and `m1-fix-960` both hold (35, 31) at frame 174; spy 2's eighth
        // knock-out — the 4 cold of `CS:30ee` — lands at 176 in `m1-fix-960` and by 177 in
        // `demo-full-24c`, and this port puts it at 175. So the *heat* is asserted on the frame
        // both recordings agree on and the knock-out's own frame is asserted (to ±1) by
        // `the_snowball_duel_knocks_out_at_the_recorded_frames`.
        assert_eq!((g.spies[0].heat, g.spies[1].heat), (35, 31), "both spies' heat words");

        // **stage 24d** — the frames the old ordering could not reach. `0x32CF` may be up to a
        // frame out of step in a dump (the recorder samples the segment asynchronously), so the
        // screen is asserted exactly and the two coordinates to ±1 — a walking spy moves one a
        // frame, so this is still a one-frame test. The walk through the door at 202 is the
        // measurement: spy 2 leaves room 6 downward through the door column and arrives on the
        // screen above at y 36, and before stage 24d this port never got there (it was four units
        // short of the column).
        // **stage 24e** — the same walk, 100 frames further: the case is taken at 485, carried to
        // the hatch room and the first part goes in at 558 (`0x2EB8` `0x400` -> `0x440`). Every
        // number here is the `demo-full-24c` dump at that frame; the port is allowed the recorder's
        // one-frame race, which is the ±1 below. Frames 500 and 590 are the ones the old port could
        // not hold — at 500 spy 1 was two units short (and from 481 his view had been frozen by a
        // visitor flag that belonged to the *other* spy), at 590 his x was one behind after the
        // deposit.
        for (f, s1, s2) in [(192u64, (6, 107, 56), (6, 140, 54)),
                            (202, (6, 104, 48), (4, 142, 38)),
                            // 211, not 210: the crossing pass takes its velocity step, so the port
                            // is one frame ahead of the dump for the length of this descent (the
                            // recorder's own race) and both are on y 54 at 211.
                            (211, (6, 104, 48), (4, 142, 54)),
                            (250, (6, 101, 54), (14, 168, 48)),
                            (383, (6, 74, 56), (4, 119, 44)),
                            (500, (6, 103, 56), (6, 114, 54)),
                            (558, (6, 60, 38), (6, 71, 36)),
                            (590, (6, 77, 54), (6, 70, 42)),
                            // **stage 24f** — past the saw hole. Spy 1 falls into it at 591 and the
                            // draw's clamp spends spy 2's reaction laugh one frame into its seed
                            // (`CS:0d44`), which gives spy 2 the whole `down` window back: 602 is
                            // his first step (`y` 44 -> 46), 608 his sixth (`y` 56, where the old
                            // port stopped at 54 and then walked into his friend's row at 613).
                            (602, (6, 78, 54), (6, 69, 46)),
                            (608, (6, 78, 54), (6, 69, 56)),
                            (613, (6, 78, 54), (6, 74, 56)),
                            // 626: spy 2 **holds** the second part on the row the descent reached —
                            // the recording's first dump that shows it (the pickup itself is inside
                            // the dumps' own 617..626 gap), and the pickup the old port never made.
                            (626, (6, 78, 54), (6, 76, 56)),
                            (636, (6, 79, 54), (6, 73, 48)),
                            (645, (6, 79, 54), (6, 73, 48)),
                            (689, (6, 72, 48), (6, 76, 48)),
                            (695, (6, 72, 56), (6, 76, 48)),
                            (706, (6, 84, 56), (6, 76, 56)),
                            // **stage 24g** — the last stretch. Spy 2 holds `right+B` from 709
                            // to 732, so he stands at x 76 while spy 1 walks the room out from
                            // under him; the recording keeps him there all the way (the port
                            // used to shove him to 90, because the walk-in-step tested A instead
                            // of B at `CS:0a65`), spy 1 takes the third part to the case at 763
                            // and climbs into the hatch room at 774.
                            //
                            // The 24g rows below are the frames where the dump and the port land
                            // on the same frame; between 707 and 734 the recorder's own sampling
                            // is a dump behind, which `stage24b_compare.py` credits with its
                            // `f`/`f-1` rule and this ±1 cannot (measured: port 114 against the
                            // dump's 112 at 721, both 140 at 774).
                            (740, (6, 144, 56), (6, 81, 48)),
                            (766, (6, 140, 46), (6, 107, 52)),
                            (774, (4, 140, 38), (6, 117, 52)),
                            (790, (4, 134, 56), (6, 132, 52))] {
            step_to(&mut g, f);
            let (p1, p2) = (pos(&g, 0), pos(&g, 1));
            for (got, want, who) in [(p1, s1, 1), (p2, s2, 2)] {
                assert_eq!(got.0, want.0, "frame {f} spy {who}'s screen");
                assert!((got.1 - want.1).abs() <= 1 && (got.2 - want.2).abs() <= 1,
                        "frame {f} spy {who}: port {got:?}, recording {want:?}");
            }
            if f == 558 {
                assert_eq!(g.mask, 0x440, "the gyroscope is in the case at 558");
                assert_eq!(g.spies[0].carried, 12, "spy 1 carries it there");
            }
            if f == 626 {
                assert_eq!(g.spies[1].carried, 12, "spy 2 holds the second part at 626");
            }
            if f == 689 {
                // the recording's own second deposit frame: `0x2EB8` `0x440` -> `0x640`
                assert_eq!(g.mask, 0x640, "the second part goes into the case at 689");
                assert_eq!(g.spies[0].carried, 12, "and spy 1 is the one who carries it");
            }
            if f == 721 {
                // **stage 24g** — the recording's visitor transfer: spy 2 is standing still on
                // B, spy 1's view slides past him, and `0x2F80` / `0x24A8` read 0 / 47 in the
                // dump (`x 76 <= map(host) 80 - 4`; his own view becomes `map - 0x21`).
                assert!(!g.spies[1].blank, "spy 2 stops being the visitor at 721");
                assert_eq!(g.spies[0].blank, false, "and spy 1 never took it");
                assert_eq!(g.spies[1].map_x, 47, "his view drops 108 -> 47 with the flag");
            }
            if f == 763 {
                // the recording's third deposit frame: `0x2EB8` `0x640` -> `0x6C0`
                assert_eq!(g.mask, 0x6C0, "the fuel canister — all three parts — at 763");
            }
        }
        // **stage 24g — the hatch at 807**, the recording's own dump (`ds_00554`): spy 1 on
        // band 4 at (116, 54), `0x32CB` = `0x32CF` = 30, the case holding `0x6C0`. Before this
        // stage the port reached the same cell at the same frame but was *refused*, because the
        // case held only `0x440` — spy 2 never walked his half of the demo.
        step_to(&mut g, 807);
        assert_eq!(pos(&g, 0), (4, 116, 54), "spy 1 on the hatch");
        assert_eq!(g.spies[0].pose_state, 30, "`0x32CF` = 30: the descent");
        assert_eq!(g.mask, 0x6C0, "all three parts in the case");
    }

    /// **Stage 24g — the demo launches, ascends for 119 frames and ends on the launch.**
    ///
    /// The recording's own last dumps (`demo-full-24c` ds_00561/ds_00562, `demo-probe-24c`'s two
    /// equivalents and `stage24c_timeline.csv`) are the whole oracle:
    ///
    /// | frame | what |
    /// |---|---|
    /// | 822 | `0x2F70 = 1` (rocket), `0x2F66 = 2` (spy 2 out), ledger `0x6C0`, clock 268 |
    /// | 822 → 941 | 119 frames of ascent, nothing else moves |
    /// | 941 | the round is over (`0x3203` → 0, `RoundEnd` = the launch, clock → 0) |
    ///
    /// The frame numbers are `DEMO.DAT`'s own replay clock (`0x0FB6 / 2`), so they join the port's
    /// `frames` directly. The port lands the launch one frame early and the end two frames early
    /// (spy 1 reaches the hatch cell on its frame 807 in both, but takes the descent on 805 — the
    /// recorder's mid-frame sampling), so the frames are asserted to ±2 and the **length** of the
    /// ascent exactly: 119 frames is `ASCENT_END / 2`, a constant of the code, not a timing.
    #[test]
    fn the_demo_launches_and_the_round_ends_on_the_launch() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let p = crate::orig::require("spy-vs-spy-iii/original/256/DEMO.DAT");
        let Ok(bytes) = std::fs::read(&p) else { return };
        let stream = round::demo_masks(&bytes);
        let mut g = game(RoundOpts {
            players: 2,
            obj_state: Some(round::RNG_ATTRACT_SEED),
            launch: true,
            demo: Some(stream),
            ..RoundOpts::default()
        });
        let step_to = |g: &mut Game, n: u64| {
            while g.frames < n {
                for _ in 0..FRAME_TICKS {
                    g.tick(Input::default());
                }
            }
        };

        // the three parts and then the hatch, so the launch is *armed*
        step_to(&mut g, 807);
        assert_eq!(g.mask, 0x6C0, "the case is full before the hatch");
        assert_eq!(g.phase, Phase::Playing, "and the round is still running");

        // the rocket: the recording's 822, and its last dump holds it set with spy 2 out
        let mut waited = 0u64;
        while !g.rocket && waited < 40 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input::default());
            }
            waited += 1;
        }
        assert!(g.rocket, "the rocket flies (`0x2F70` = 1)");
        assert!(g.spies[1].out_of_round, "spy 2 is out of the round (`0x2F66` = 2)");
        let launched = g.frames;
        let clock = g.clock_secs();
        assert!((819..=827).contains(&launched), "launch on frame {launched}");
        assert!((256..=272).contains(&clock), "clock {clock} s at the launch");

        // the ascent: both spies frozen, nothing else happening, until `0x248A` ends the round
        let mut n = 0u64;
        while g.phase == Phase::Playing && n < 400 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input::default());
            }
            n += 1;
        }
        assert_eq!(g.phase, Phase::Over, "the round ends");
        assert_eq!(g.end, Some(RoundEnd::Launch), "on the launch (`CS:0649`)");
        // `ASCENT_END` = 0xEE = 238 is reached in `+2` a frame, so the launch frame itself carries
        // the first increment and the flag lands `ASCENT_END / 2 - 1` frames later: 118 intervals,
        // 119 increments — the recording's 822 -> 941 (its round-over dump is one sample late).
        assert_eq!(n, (round::ASCENT_END / 2) as u64 - 1,
                   "the ascent: {} increments of `0x2EBC`", round::ASCENT_END / 2);
        assert!((935..=945).contains(&g.frames),
                "round over on frame {} (the recording's 941)", g.frames);
    }

    /// **Stage 24f — a reaction is ten frames whichever spy it lands on, because the seed frame is
    /// always spent in the frame it is armed.**
    ///
    /// `FUN_26c0_277c` seeds every animation at `0x2EFC = -2` and the frame loop runs **player 2
    /// first** (`CS:0702`), so the two spies reach that seed by different routes:
    ///
    /// * a laugh armed on **player 1** (by player 2's hazard) is stepped by player 1's own body in
    ///   the same frame (`CS:0982` → `CS:098c`), so `0x2EFC` reads 0 at the end of the arming frame
    ///   and the first *stepped* frame is 2;
    /// * a laugh armed on **player 2** (by player 1's hazard) is armed after his body has run, so
    ///   only the draw's `CS:0d44` touches the word — and it writes the same 0.
    ///
    /// Both are therefore **10 frames** of raise from the arming frame, and both are free on the
    /// eleventh. The port used to give player 2 **11**, because nothing spent his seed: the extra
    /// frame is the whole of the demo's saw-hole slip (the recording's spy 2 is back on his keys at
    /// 602, the last frame of the `down` window he needs, and the port had him raised until 603).
    /// The recording's `demo-full-24c` `ds_00400` — spy 1's fall, the frame spy 2 is armed — reads
    /// `0x2EFE = 0` where the unspent seed reads `0xFFFE`.
    #[test]
    fn a_reaction_laugh_is_ten_frames_on_either_spy() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for victim in 0..2usize {
            let other = 1 - victim;
            let mut g = two_at(0, 4, 120, 44, 150);
            put_underfoot_for(&mut g, victim, items::OBJ_HOLE);
            // the hazard fires on the frame *after* the one the byte appears on (the feet cell is the
            // one the previous frame's end cached, `CS:0afd` -> `0x2549`)
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.spies[victim].anim.map(|a| a.state), Some(round::STATE_FALL),
                       "P{} fell into the hole", victim + 1);
            let armed = g.frames;
            assert_eq!(g.spies[other].act_state, round::STATE_LAUGH,
                       "P{} laughs at him (`CS:24de`)", other + 1);
            assert_eq!(g.spies[other].anim.map(|a| a.state), Some(round::STATE_LAUGH));
            // the seed frame is spent by whoever gets to it first
            assert_eq!(g.spies[other].anim.map(|a| a.frame), Some(0),
                       "P{}: the seed frame reads 0 after the frame it is armed in", other + 1);
            assert_eq!(g.spies[other].frame, 0, "`0x2EFC` is the same word the draw spends");
            // the next frame steps it to 2, i.e. the drawn frame 0 is never stepped
            frame2(&mut g, NOKEYS, NOKEYS);
            assert_eq!(g.spies[other].anim.map(|a| a.frame), Some(2),
                       "P{}: the first stepped frame is 2, not 0", other + 1);
            // and the whole raise
            let mut n = 1u64;
            while g.spies[other].anim.is_some() && n < 40 {
                frame2(&mut g, NOKEYS, NOKEYS);
                n += 1;
            }
            let raised = g.frames - armed;
            assert_eq!(raised, 10,
                       "P{other}'s laugh raise, armed by P{victim}: {} frames",
                       raised, other = other + 1, victim = victim + 1);
        }
    }

    /// **Stage 24d — the snowball duel's knock-out chain, at the frames the dumps hold.**
    ///
    /// From frame 61 the two recorded spies stand eighteen units apart and hold button B with a
    /// direction for the whole of `DEMO.DAT`'s duel (`0x2822` for a hundred frames at a time), so
    /// they pelt each other point-blank and neither can walk. The knock-out frames below are read
    /// out of the dumps — `0x32CF` = `0x16` with `0x2F18` set, dated to the frame the reaction
    /// starts (the seed frame `0x2EFC = -2` where a dump caught it, otherwise the first drawn
    /// frame minus one). The dump ring is ~1.7 frames apart, so every one of them is ±1:
    ///
    /// | | recorded knock-outs (`m1-fix-960`, `demo-full-24c`, `demo-probe-24c`) |
    /// |---|---|
    /// | spy 1 | 78, 87, 103, 112, 127, 138, 152, 163, 178 |
    /// | spy 2 | 91, 100, 116, 125, 141, 150, 165, 176 |
    ///
    /// With the ball pass after *both* frame bodies (stage 16b's reading of `CS:e6a`) this port's
    /// chain ran a **26-frame** cycle against the recording's 25 and was four frames behind by
    /// 178 — the reason spy 2 was still inside an animation at frame 180, so his first step came
    /// at 183 and every later mask in `DEMO.DAT` landed on a spy in the wrong place. The chain is
    /// the oracle to use here because while both spies are pinned this is the only state that can
    /// be measured at all.
    #[test]
    fn the_snowball_duel_knocks_out_at_the_recorded_frames() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let p = crate::orig::require("spy-vs-spy-iii/original/256/DEMO.DAT");
        let Ok(bytes) = std::fs::read(&p) else { return };
        let stream = round::demo_masks(&bytes);
        let mut g = game(RoundOpts {
            players: 2,
            obj_state: Some(round::RNG_ATTRACT_SEED),
            launch: true,
            demo: Some(stream),
            ..RoundOpts::default()
        });
        let knock = |g: &Game, i: usize| {
            matches!(g.spies[i].anim.map(|a| a.state),
                     Some(round::STATE_KNOCK_FACE) | Some(round::STATE_KNOCK_BACK))
        };
        let mut seen: [Vec<u64>; 2] = [Vec::new(), Vec::new()];
        let mut was = [false, false];
        while g.frames < 196 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input::default());
            }
            for i in 0..2 {
                let now = knock(&g, i);
                if now && !was[i] {
                    seen[i].push(g.frames);
                }
                was[i] = now;
            }
        }
        let want: [&[u64]; 2] = [
            &[78, 87, 103, 112, 127, 138, 152, 163, 178],
            &[91, 100, 116, 125, 141, 150, 165, 176],
        ];
        for i in 0..2 {
            assert_eq!(seen[i].len(), want[i].len(),
                       "spy {}: the recording's {} knock-outs, the port's {:?}", i + 1,
                       want[i].len(), seen[i]);
            for (got, &w) in seen[i].iter().zip(want[i]) {
                assert!((*got as i64 - w as i64).abs() <= 1,
                        "spy {}: knock-out at {got}, the recording's is at {w} (±1); the port's \
                         whole chain is {:?}, the recording's {want:?}",
                        i + 1, seen[i]);
            }
        }
    }

    /// **Stage 24b — nobody launches inside the recording's window, and that is a fact about the
    /// dumps, not about the port.**
    ///
    /// In all 244 demo dumps of both sessions `0x2F64`, `0x2F66` (out of the round), `0x2F70` (the
    /// rocket flying) and `0x248A` (the ascent) are **zero** at every frame up to 383, and
    /// `0x324D` is still **317 s**. So the recording can say nothing about a launch: it stops 41 s
    /// into a demo that runs to a clock of 360. This test pins the part that *is* recorded — the
    /// round is still playing at the last recorded frame, with both spies in it. (Stage 24c then
    /// recorded the whole demo: the launch is at frame 822 and the round ends on it at 941 — see
    /// `the_demo_launches_and_the_round_ends_on_the_launch`, which is the other half of this.)
    #[test]
    fn the_demo_is_still_playing_where_the_recording_stops() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let masks: Vec<u16> = vec![0u16; 400];
        let mut g = game(RoundOpts {
            players: 2,
            obj_state: Some(round::RNG_ATTRACT_SEED),
            launch: true,
            demo: Some(masks),
            ..RoundOpts::default()
        });
        while g.frames < 383 {
            for _ in 0..FRAME_TICKS {
                g.tick(Input::default());
            }
        }
        assert_eq!(g.end, None, "no exit at frame 383: 0x2F70/0x248A are zero in every dump");
        assert!(g.spies.iter().all(|s| !s.launched && !s.out_of_round));
        // `0x324D` = 317 at the last demo dump of both sessions (frame 383); the port's own count
        // is one frame period longer per second (2.5 %, STAGE24A §1.4), so two counts is the
        // documented tolerance rather than an exact word.
        let rec_clock = 317i64;
        assert!(
            (g.clock_secs() as i64 - rec_clock).abs() <= 2,
            "DS:0x324D at frame 383: port {} vs the recording's {rec_clock}",
            g.clock_secs()
        );
    }

    /// **v0.1.4 — an all-zero `spy.exe` cannot hang `--scene round`.**
    ///
    /// The security review's repro: the right number of bytes, every one zero, and `Game::new`
    /// never returned. `place_igloo_fires` was re-rolling a level table that holds no bare
    /// interior for ever. It is bounded now, so this either starts a (nonsense) round or reports a
    /// clean `Err`; the wall-clock assertion is the defect itself, since a regression hangs here
    /// rather than failing an equality.
    #[test]
    fn an_all_zero_spy_exe_starts_or_errors_without_hanging() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = std::fs::read(crate::orig::require("spy-vs-spy-iii/original/256/spy.exe"))
            .expect("spy.exe");
        let zero = vec![0u8; exe.len()];
        let t = std::time::Instant::now();
        match Game::new(zero, &RoundOpts::default()) {
            Ok(mut g) => {
                for _ in 0..3 {
                    g.tick(Input::default());
                }
                let _ = g.frame(false);
            }
            Err(e) => assert!(!e.is_empty(), "an error says what is wrong"),
        }
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "a zeroed spy.exe took {:?}: the round start is not bounded",
            t.elapsed()
        );
    }

}
