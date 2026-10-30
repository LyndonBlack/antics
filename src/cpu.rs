//! The **computer opponent** — the original's own decision routine, ported.
//!
//! Everything here comes from `spy.exe`'s second code segment: the routine at `CS:129c` (the
//! per-frame driver), `CS:1ca9` (the goal selector), and the six behaviours it dispatches to
//! (`CS:1aa8`, `CS:1b5a`, `CS:1c4d`, `CS:1574`, `CS:1a01`, `CS:1886`) with the navigation helpers
//! `CS:164c`/`CS:1774`. See `recon/round/port_check/STAGE17_REPORT.md`.
//!
//! # Where the AI runs in the original
//!
//! `CS:0325` is the frame's input scan: it clears both players' key-flag words, copies the keyboard
//! and joystick bits into them, and then — **when `DS:0x31DD == 1`** (the options screen's "one
//! spy" setting, `CS:1204` copies the menu's `0x31DB` into it at GO!) — it does *not* copy player
//! 2's keys and instead calls `CS:129c` with `SI = 2`:
//!
//! ```text
//!  0348  if (word[0x31dd] == 1) { lea 0x33be,%si ... ; jmp 03a2 }
//!  039c  mov  $2,%si
//!  039f  call 0x129c                  ; the computer plays spy 2
//! ```
//!
//! So in the original the computer is **always the black spy**; a one-player round is the only
//! mode that has one, and the AI writes exactly the same six key-flag words the keyboard handler
//! writes (button A, button B, up, down, left, right — at `DS:0x327F+si` .. `0x3293+si`, a table of
//! *pointers* to per-player words). Everything downstream — the walk, the trapulator, the throw —
//! cannot tell the difference, which is why this port can reuse every rule it already has.
//!
//! The port keeps that shape: [`Cpu::think`] returns a [`Keys`] value for one game frame, and the
//! engine substitutes it for the human's keys. `--cpu1` exists only so the AI can be exercised as
//! the white spy too (the original's code is `SI`-relative throughout, so it is symmetric; nothing
//! in the original ever calls it with `SI = 0`).
//!
//! # Words this module deliberately does *not* model
//!
//! * **`DS:0x34A0` / `DS:0x3496`** — retired in stage 18d: they are the **PC-speaker sequencer's**
//!   state word and pending-cue pointer, inside the INT 1Ch handler at `CS:4196` (installed by
//!   `CS:4548`, its `DS` immediate patched by the MZ relocation at `CS:419f`, so it is the game's
//!   own segment; `0x34A0` is 0 idle and 1..4 the note envelope's phases, `0x3496` a pointer that
//!   the handler copies to `0x3494`). Game code touches `0x34A0` only as a 19x "may I pre-empt?"
//!   gate (`cmpw $0x2`) around exactly the two cue stores. **Nothing in the AI reads either word**:
//!   the cues beside the panel, the throw and the stow change which note plays and never a game
//!   word. Sound is not ported in this milestone, so the block has no counterpart here and the AI
//!   must decide the same way whether a cue is playing or not (test:
//!   `the_speaker_state_is_not_an_ai_input`).
//! * **`DS:0x2F96`** — the *white* spy's half of the slot pair `0x2F96`/`0x2F98` (see
//!   [`Cpu::target_slot`]). `0x2F98` is the AI's own request; `0x2F96` is written by nothing in the
//!   whole image and is 0 in every recorded dump, so the port never needs a constant for it — the
//!   dump reader indexes the pair with the spy's `si`.

use crate::game::{Game, Keys, Rng};
use crate::items;
use crate::round;

/// `DS:0x316C` — the five difficulty thresholds (word per setting). A **random byte must reach the
/// threshold** for the AI's item/event tests to fire, so the table runs *down*: 254 at difficulty 1
/// (0.8 % per test) to 180 at difficulty 5 (30 %). This is the reaction/aggression dial.
pub const DIFF_THRESHOLDS: [u16; 5] = [180, 205, 225, 250, 254];

/// `DS:0x31CB` — the five **cold thresholds**, compared with the spy's own cold word (`0x2F5A`).
/// 42 at difficulty 1 against 34 at difficulty 5: the harder the setting, the earlier the computer
/// breaks off what it is doing to warm up.
pub const DIFF_COLD: [u16; 5] = [42, 40, 38, 36, 34];

/// `DS:0x31E1` — the per-difficulty record drawn at the round start. Read, not used by the AI; it
/// lives here so the three difficulty tables are in one place.
// `#[allow(dead_code)]`: kept because it documents `DS:0x31E1`, the fifth difficulty table, beside
// the two the AI does read (`DIFF_THRESHOLDS`, `DIFF_COLD`); nothing needs it at run time.
#[allow(dead_code)]
pub const DIFF_ART: [u16; 5] = [0x7330, 0x7362, 0x7394, 0x73C6, 0x7410];

/// The AI's own action codes (`DS:0x2F90`), read off `CS:1ca9`'s tail and the behaviours.
// `#[allow(dead_code)]`: `ACT_ATTACK` and `ACT_NAV` complete the `0x2F90` action enumeration the
// AI's table stores (`ACT_CHASE`/`ACT_EXIT`/`ACT_WARM` are read); the code compares against the
// latter three, so the first two are documentation of the same byte.
#[allow(dead_code)]
pub const ACT_ATTACK: u16 = 0; // 0 and 1 (the byte table's own 0/1) -> attack the other spy
#[allow(dead_code)]
pub const ACT_NAV: u16 = 2; // 2 and 3 -> walk to the hatch
pub const ACT_CHASE: u16 = 4; // walk to the other spy
pub const ACT_EXIT: u16 = 5; // walk to a part / the exit door
pub const ACT_WARM: u16 = 6; // find the igloo

/// The six intent words, in the memory order `CS:1401..1447` dispatches them (`DS:0x2ECE`,
/// `0x2ED2`, `0x2EDA`, `0x2EC6`, `0x2ED6`, `0x2ECA`).
pub const I_MAP: usize = 0;
pub const I_ITEM: usize = 1;
pub const I_GRAB: usize = 2;
pub const I_NAV: usize = 3;
pub const I_ATTACK: usize = 4;
pub const I_WARM: usize = 5;

/// `DS:0x30AC`, transcribed from the executable (the test `the_action_table_is_the_executables_own`
/// checks it byte for byte). Three 0x30-byte bands — apart, shared, and the two last-minute
/// variants — each 3 rows of 16 by (my hand, his hand).
pub const ACTION_TABLE: [u8; 0xC0] = [
    5, 5, 9, 9, 5, 5, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, //
    5, 5, 5, 9, 5, 9, 4, 9, 4, 2, 9, 9, 2, 9, 9, 9, //
    5, 9, 9, 3, 9, 9, 9, 9, 9, 9, 9, 9, 2, 9, 9, 9, //
    1, 1, 9, 9, 1, 1, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, //
    0, 1, 0, 9, 1, 9, 0, 9, 0, 0, 9, 9, 9, 9, 9, 9, //
    5, 9, 9, 3, 9, 9, 9, 9, 9, 9, 9, 9, 0, 9, 9, 9, //
    5, 5, 9, 9, 5, 5, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, //
    5, 5, 5, 9, 5, 9, 4, 9, 4, 2, 9, 9, 9, 9, 9, 9, //
    5, 9, 9, 3, 9, 9, 9, 9, 9, 9, 9, 9, 3, 9, 9, 9, //
    1, 1, 9, 9, 1, 0, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, //
    0, 1, 0, 9, 1, 9, 2, 9, 0, 0, 9, 9, 9, 9, 9, 9, //
    5, 9, 9, 3, 9, 9, 9, 9, 9, 9, 9, 9, 0, 9, 9, 9, //
];

/// `DS:0x3176` — the kit bit each object code owns (`items::code_bit`); 0xFFFF = "not an item".
pub const CODE_BITS: [u16; 13] = [0, 0, 1, 2, 0, 4, 8, 0x10, 0x20, 0, 0, 0, 0xFFFF];

/// `DS:0x2D60` — kit/dispatch bit per item code, read through `items::code_bit`.
const DS_ACTION_TABLE: usize = 0x30AC;

/// One of the four links the round start records (`DS:0x3219`/`0x3205`/`0x320F` indexed by
/// `2*(code-8)`): entry 0 is the **hatch**, entries 1..3 are the three rocket parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub used: bool,
    /// `DS:0x3219` — the screen (`2 * block index`, the engine's own screen word).
    pub screen: u16,
    /// `DS:0x3205` — the object row band 0..3.
    pub row: u8,
    /// `DS:0x320F` — the object **entry** (`x >> 1`), not a world x.
    pub col: u8,
}

/// `DS:0x3176` is the **black** spy's item table (the original drives only black): the white
/// plunger (1) and bucket (4) are 0 in it, the black ones (2, 5) carry the kit bits. The port's
/// white computer swaps the pairs, or it would pick the black spy's bucket up, find no slot for it,
/// drop it and pick it up again for ever (a port-only mode, evidence: read + soak).
pub fn colour_item_table(mut t: [u16; 13], player: usize) -> [u16; 13] {
    if player == 0 {
        t.swap(1, 2);
        t.swap(4, 5);
    }
    t
}

impl Default for Link {
    /// An invalid record: `DS:0x3219 = 0xFFFF` (what `CS:383f` writes when the item is taken).
    fn default() -> Link {
        Link { used: false, screen: 0xFFFF, row: 0, col: 0 }
    }
}

/// Everything the AI may read, copied out of the engine for one frame. Building this instead of
/// passing `&Game` keeps the routine a pure function — the tests drive it without a round.
#[derive(Clone, Debug)]
pub struct World {
    pub difficulty: u16,
    /// `DS:0x2EB8` — the deposited-parts ledger. Its 0x40/0x80/0x200 bits are the three parts,
    /// which is exactly what `CS:1ca9` counts.
    pub mask: u16,
    /// `DS:0x324D` — the round clock, in counts.
    pub clock_left: u16,
    /// `DS:0x2FB0` — how many igloo interiors got a fire this round.
    pub fires: u16,
    pub visitors: [i16; 2],
    pub sink: [i16; 2],
    pub frozen: [i16; 2],
    pub anim: [bool; 2],
    pub carried: [i16; 2],
    pub heat: [i16; 2],
    pub pose_state: [i16; 2],
    pub kit: [u16; 2],
    /// `DS:0x330F + si` — sticks in the trapulator.
    pub dyn_sticks: [u16; 2],
    /// `DS:0x2F8A + si` — **"this spy has taken the briefcase this round"**, not "is carrying it".
    /// It is a **latch**: `CS:3890` (`movw $0x1,0x2f8a(%si)`) sets it in the shared pickup tail for
    /// a picked-up code `>= 0x0C` — the briefcase, the only code that gets there (`CS:376b` refuses
    /// everything above) — and the only thing that clears it is the per-round template restore
    /// `CS:125f` (`rep movsw` of 250 bytes from `DS:0x2FB2` over `0x2EB8`). Dropping or stowing the
    /// case does **not** clear it: of the 1,784 recorded dumps with the black half set, the case is
    /// in black's hand in only 787 (44 %).
    ///
    /// Exactly two readers, both the AI's: `CS:14d5` (a part under the feet is no longer worth a
    /// dynamite trap once the case has been mine — see [`Cpu::look_for_item`]) and `CS:1daa` (the
    /// cold branch: with two parts in the case, the case in neither hand and the latch set, go to
    /// the exit (5) instead of warming up (6) — see [`Cpu::decide`]).
    pub case_owned: [bool; 2],
    /// Would `CS:315f` (the stow) leave this spy's hand empty? The original calls it and looks at
    /// the hand on the very next instruction; the engine applies the stow after `think`, so the
    /// answer is computed beforehand (a drop is refused indoors and above y = 0x24).
    pub stow_ok: [bool; 2],
    pub tag: [u16; 2],
    pub x: [i32; 2],
    pub y: [i32; 2],
    pub screen: [u16; 2],
    /// The spy's object cell, as `(cell - block) / 156` and `% 156` — `CS:164c`'s row and column.
    pub cell_row: [i16; 2],
    pub cell_col: [i16; 2],
    /// The object byte under each spy's feet (`DS:0x32D9`'s target).
    pub cell: [u8; 2],
    /// The object byte one entry west / east of the feet (for `CS:1774`).
    pub ahead_w: [u8; 2],
    pub ahead_e: [u8; 2],
    /// `DS:0x2FAA` — the terrain code under the feet.
    pub ground: [i16; 2],
    pub trap_open: [bool; 2],
    pub trap_cursor: [u16; 2],
    pub act_latch: [bool; 2],
    pub blocked: [bool; 2],
    pub links: [Link; 5],
    /// `DS:0x2536 + 24*(screen/2)` — the igloo interior paired with each outdoor screen: rows 5..9
    /// of the level table, twelve columns of level codes.
    pub interiors: [[u16; 12]; 5],
    /// The whole 10x12 level table (`DS:0x24BE`), for the door scans in `CS:164c`.
    pub levels: [[u16; 12]; 10],
    /// `DS:0x25AE` — the twelve gate columns.
    pub gate: [u16; 12],
    pub thresholds: [u16; 5],
    pub cold: [u16; 5],
    /// `DS:0x30AC` — the 192-byte action table, verbatim from the executable.
    pub actions: [u8; 0xC0],
    /// `DS:0x3176` — the object code's kit bit (`items::code_bit`), 0..12.
    pub code_bits: [u16; 13],
}

/// The five-entry position record at round start: the hatch, then the three parts and the case by
/// item code (`CS:25ac..25cb`, `CS:265d`). The buried bit (0x80) of the stored byte is set *after*
/// the record is made, so a buried item is linked all the same.
pub fn links_from_layout(layout: &round::ObjectLayout) -> [Link; 5] {
    let mut links = [Link::default(); 5];
    if let Some(h) = layout.hatch {
        links[0] = Link { used: true, screen: h.band, row: h.row as u8, col: h.entry as u8 };
    }
    for o in &layout.objects {
        let code = o.code & 0x7f;
        if (9..=12).contains(&code) {
            links[(code - 8) as usize] =
                Link { used: true, screen: o.band, row: o.row as u8, col: o.entry as u8 };
        }
    }
    links
}

impl World {
    /// Snapshot the parts of a live round the AI reads.
    ///
    /// `player` is the spy the AI drives. The object cell, the byte ahead and the terrain code are
    /// the engine's own (`items::cell_addr`, `terrain::code`), which is exactly what `DS:0x32D9`
    /// and `DS:0x2FAA` hold in the original.
    pub fn from_game(g: &Game, player: usize) -> World {
        let mut w = World::default();
        w.difficulty = g.difficulty.min(4);
        w.mask = g.mask;
        w.clock_left = g.clock_left;
        w.fires = g.fires;
        w.levels = g.levels;
        for s in 0..5 {
            w.interiors[s] = g.levels[5 + s];
        }
        for c in 0..12 {
            w.gate[c] = g.terrain.gate(c) as u16;
        }
        // tables come from the executable's image; the *object bytes* must come from the live
        // segment, or the AI reads the wrong cell (this is what made the first soak loop on a saw
        // that was not there).
        let ds = &g.exe[crate::game::DS_FILE_OFF..];
        let live = g.ds_ref();
        for k in 0..5 {
            w.thresholds[k] = u16::from_le_bytes([
                ds[DS_ACTION_TABLE + 0xC0 + 2 * k],
                ds[DS_ACTION_TABLE + 0xC0 + 2 * k + 1],
            ]);
            w.cold[k] = u16::from_le_bytes([
                ds[DS_ACTION_TABLE + 0x11F + 2 * k],
                ds[DS_ACTION_TABLE + 0x11F + 2 * k + 1],
            ]);
        }
        w.actions.copy_from_slice(&ds[DS_ACTION_TABLE..DS_ACTION_TABLE + 0xC0]);
        for c in 0..13u16 {
            w.code_bits[c as usize] = word(ds, 0x3176 + 2 * c as usize);
        }
        w.code_bits = colour_item_table(w.code_bits, player);
        for (i, spy) in g.spies.iter().enumerate() {
            w.visitors[i] = spy.blank as i16;
            w.sink[i] = spy.pose_clip as i16;
            w.frozen[i] = spy.frozen as i16;
            w.anim[i] = spy.anim.is_some() || spy.pose_raised;
            w.carried[i] = if spy.carried == crate::game::CARRY_EMPTY {
                -1
            } else {
                spy.carried as i16
            };
            w.heat[i] = spy.heat as i16;
            w.pose_state[i] = spy.pose_state as i16;
            w.kit[i] = spy.kit;
            w.case_owned[i] = spy.case_owned;
            w.stow_ok[i] = g.stow_empties_hand(i);
            w.dyn_sticks[i] = spy.dyn_sticks as u16;
            w.tag[i] = items::player_tag(ds, i) as u16;
            w.x[i] = spy.x();
            w.y[i] = spy.y();
            w.screen[i] = spy.screen;
            w.trap_open[i] = spy.trap_open;
            w.trap_cursor[i] = spy.trap_cursor;
            w.act_latch[i] = spy.act_latch;
            w.blocked[i] = spy.blocked;
            w.ground[i] = g
                .terrain
                .code(&g.levels, spy.screen, spy.x(), spy.y())
                .map(|c| c as i16)
                .unwrap_or(-1);
            if let Some(addr) = items::cell_addr(live, spy.screen, spy.x(), spy.y()) {
                w.cell[i] = live[addr];
                w.cell_col[i] = (addr % 0x9C) as i16;
                if let Some(block) = round::block_addr(live, spy.screen) {
                    let lin = addr - block;
                    w.cell_row[i] = (lin / 0x9C) as i16;
                    w.cell_col[i] = (lin % 0x9C) as i16;
                }
            }
            if let Some(addr) = items::cell_ahead(live, spy.screen, spy.x(), spy.y(), true) {
                w.ahead_w[i] = live[addr];
            }
            if let Some(addr) = items::cell_ahead(live, spy.screen, spy.x(), spy.y(), false) {
                w.ahead_e[i] = live[addr];
            }
        }
        // the position record the engine keeps (round start, pickups, drops)
        let links = g.links;
        w.links = links;
        w
    }
}

/// A word out of a 64 KB data-segment dump, with the original's wrap at the segment end.
fn word(ds: &[u8], a: usize) -> u16 {
    let a = a & 0xFFFF;
    u16::from_le_bytes([ds[a], ds[(a + 1) & 0xFFFF]])
}
fn sword(ds: &[u8], a: usize) -> i16 {
    word(ds, a) as i16
}

impl World {
    /// The AI's view of a **recorded** frame, straight out of a 64 KB dump, reading the original's
    /// own words. This is what the corpus check uses: it makes the decision code testable against
    /// the game's state without the port's model in between.
    pub fn from_dump(ds: &[u8], player: usize, exe: &[u8]) -> World {
        let mut w = World::default();
        let d = &exe[crate::game::DS_FILE_OFF..];
        w.difficulty = (word(ds, 0x31bf) / 2).min(4);
        w.mask = word(ds, 0x2eb8);
        w.clock_left = word(ds, 0x324d);
        w.fires = word(ds, 0x2fb0);
        for i in 0..2 {
            let o = i * 2;
            w.visitors[i] = sword(ds, 0x2f7e + o);
            w.sink[i] = sword(ds, 0x2f68 + o);
            w.frozen[i] = sword(ds, 0x2f64 + o);
            w.anim[i] = sword(ds, 0x2f18 + o) != 0;
            w.carried[i] = sword(ds, 0x2f2c + o);
            w.heat[i] = sword(ds, 0x2f58 + o);
            w.pose_state[i] = sword(ds, 0x32cf + o);
            w.kit[i] = word(ds, 0x32a3 + o);
            w.dyn_sticks[i] = word(ds, 0x330f + o);
            w.case_owned[i] = word(ds, 0x2f8a + o) != 0;
            // a recording cannot say what the stow would do; the one refusal that needs no
            // inventory is the drop guard (indoors, or above y = 0x24)
            w.stow_ok[i] = !(w.screen[i] >= 10 || w.y[i] < 0x24) || w.carried[i] == -1;
            w.tag[i] = items::player_tag(ds, i) as u16;
            w.x[i] = word(ds, 0x329b + o) as i32;
            w.y[i] = word(ds, 0x3297 + o) as i32;
            w.screen[i] = word(ds, 0x329f + o);
            w.ground[i] = sword(ds, 0x2faa + o);
            w.trap_open[i] = word(ds, 0x2f38 + o) != 0;
            w.trap_cursor[i] = word(ds, 0x2f3c + o);
            w.act_latch[i] = word(ds, 0x2f28 + o) != 0;
            w.blocked[i] = word(ds, 0x2fa4 + o) != 0;
            let cell = word(ds, 0x32d7 + o) as usize;
            w.cell[i] = ds[cell & 0xFFFF];
            if let Some(base) = round::block_addr(ds, w.screen[i]) {
                if cell >= base && cell - base < 4 * 0x9c {
                    w.cell_row[i] = ((cell - base) / 0x9c) as i16;
                    w.cell_col[i] = ((cell - base) % 0x9c) as i16;
                }
            }
            w.ahead_w[i] = ds[(cell.wrapping_sub(1)) & 0xFFFF];
            w.ahead_e[i] = ds[(cell + 1) & 0xFFFF];
        }
        for c in 0..12 {
            w.gate[c] = word(ds, 0x25ae + 2 * c);
        }
        for r in 0..10 {
            for c in 0..12 {
                w.levels[r][c] = word(ds, 0x24be + 24 * r + 2 * c);
            }
        }
        for r in 0..5 {
            w.interiors[r] = w.levels[5 + r];
        }
        for bp in 0..5 {
            let o = bp * 2;
            w.links[bp] = Link {
                used: sword(ds, 0x3219 + o) >= 0,
                screen: word(ds, 0x3219 + o),
                row: ds[0x3205 + o],
                col: ds[0x320f + o],
            };
        }
        for k in 0..5 {
            w.thresholds[k] = word(d, 0x316c + 2 * k);
            w.cold[k] = word(d, 0x31cb + 2 * k);
        }
        w.actions.copy_from_slice(&d[0x30ac..0x316c]);
        for c in 0..13u16 {
            w.code_bits[c as usize] = word(d, 0x3176 + 2 * c as usize);
        }
        w.code_bits = colour_item_table(w.code_bits, player);
        w
    }

    /// The AI's own words out of a dump: the six intents, the countdown, the action, the slot it
    /// wants and the two retry counters. Seeding them is what lets a probe ask "what would the
    /// routine do on the *next* frame of the recording?".
    pub fn cpu_from_dump(ds: &[u8], player: usize) -> Cpu {
        let o = player * 2;
        let mut c = Cpu::new(player, (word(ds, 0x31bf) / 2).min(4));
        for (i, a) in [0x2eceusize, 0x2ed2, 0x2eda, 0x2ec6, 0x2ed6, 0x2eca].iter().enumerate() {
            c.intent[i] = sword(ds, a + o);
        }
        c.countdown = sword(ds, 0x3243);
        c.action = word(ds, 0x2f90);
        // `CS:1c2b`'s `cmpw $0xa,0x2f96(%si)` and `CS:1bf4`'s `cmp 0x2f98,%ax` both read the
        // **per-spy pair** `0x2F96`/`0x2F98`: the black spy's half (0x2F98) is the slot the AI
        // writes for itself (`CS:13bf` 0x0a, `CS:13d1` 0x04, `CS:13e3` 0x02, `CS:14fb` 0x0a) and
        // zeroed by the round restore `CS:125f`; the white half (0x2F96) is written by nothing in
        // the whole image and is 0 in all 19,425 recorded dumps.
        c.target_slot = word(ds, 0x2f96 + o);
        c.slot_confirm = word(ds, 0x2f9c);
        c.retry = word(ds, 0x2fa2);
        c.cooldown = sword(ds, 0x2f94);
        c.found = word(ds, 0x2fa8);
        c.can_act = word(ds, 0x2f88);
        c
    }
}

impl Default for World {
    fn default() -> Self {
        World {
            difficulty: 0,
            mask: 0x400,
            clock_left: 360,
            fires: 1,
            visitors: [0, 0],
            sink: [0, 0],
            frozen: [0, 0],
            anim: [false, false],
            carried: [-1, -1],
            heat: [3, 3],
            pose_state: [0, 0],
            kit: [0, 0],
            dyn_sticks: [0, 0],
            case_owned: [false, false],
            stow_ok: [true, true],
            tag: [0x20, 0x40],
            x: [100, 120],
            y: [46, 46],
            screen: [4, 4],
            cell_row: [2, 2],
            cell_col: [50, 60],
            cell: [0, 0],
            ahead_w: [0, 0],
            ahead_e: [0, 0],
            ground: [0, 0],
            trap_open: [false, false],
            trap_cursor: [0, 0],
            act_latch: [false, false],
            blocked: [false, false],
            links: [Link::default(); 5],
            interiors: [[0x14; 12]; 5],
            levels: [[0; 12]; 10],
            gate: [0, 26, 52, 78, 104, 130, 156, 182, 208, 234, 260, 286],
            thresholds: DIFF_THRESHOLDS,
            cold: DIFF_COLD,
            actions: ACTION_TABLE,
            code_bits: CODE_BITS,
        }
    }
}

/// A draw from `FUN_26c0_20cd` — the low byte is what every caller tests.
pub trait Drawer {
    fn draw(&mut self) -> u16;
}

impl Drawer for Rng {
    fn draw(&mut self) -> u16 {
        (self.next() & 0xFF) as u16
    }
}

/// The two writes the AI makes that are not key presses: it opens the trapulator with its own
/// hand (`CS:1b0e`, `CS:1be4`) and sets the put-away latch (`CS:1c68`, `CS:156d`), and it stows
/// through `FUN_26c0_315f` (`CS:1acd`, `CS:1b9a`, `CS:1a0e`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Side {
    pub trap_open: Option<bool>,
    /// **stage 20c** — `DS:0x2F3C + si`: the trapulator cursor the errand places on the slot it
    /// asked for (`0x2F98`) on the frame it opens the panel. The walk the panel's repeat counter
    /// (`DS:0x2F9E`) would take three frames a slot for is what the recording does not show.
    pub trap_cursor: Option<u16>,
    pub act_latch: Option<bool>,
    pub stow: bool,
}

/// The computer spy.
#[derive(Clone, Debug)]
pub struct Cpu {
    pub player: usize,
    /// The setting, 0..4 (`DS:0x31BF / 2`).
    pub difficulty: u16,
    /// The six intent/phase words, one per behaviour.
    pub intent: [i16; 6],
    /// `DS:0x3243` — the decision countdown.
    pub countdown: i16,
    /// `DS:0x2F90` — the action the selector chose.
    pub action: u16,
    /// `DS:0x2F96 + si` — the trapulator slot the AI asked for: `0x0A` the dynamite, `0x04` the
    /// saw, `0x02` the bucket (`CS:13bf`/`13d1`/`13e3`, and `CS:14fb`'s dynamite request). The
    /// black spy's half is `0x2F98`; `0x2F96` is the white spy's and is written by nothing in the
    /// original. Read back by the item behaviour's tail (`CS:1c2b`) and by the cursor compare
    /// (`CS:1bf4`).
    pub target_slot: u16,
    /// `DS:0x2F9C` — "the slot is already chosen, go straight to the panel".
    pub slot_confirm: u16,
    /// `DS:0x2FA2` — the wall-bump retry timer.
    pub retry: u16,
    /// `DS:0x2F94` — the cooldown that gates the idle fidget.
    pub cooldown: i16,
    /// `DS:0x2FA8` — the last object code the item test looked at.
    pub found: u16,
    /// `DS:0x2F88` — never written outside `CS:1ca9`'s clear in this segment; kept for the record.
    pub can_act: u16,
}

impl Cpu {
    pub fn new(player: usize, difficulty: u16) -> Cpu {
        Cpu {
            player,
            difficulty: difficulty.min(4),
            intent: [0; 6],
            countdown: 0,
            action: 0,
            target_slot: 0,
            slot_confirm: 0,
            retry: 0,
            cooldown: 0,
            found: 0,
            can_act: 0,
        }
    }

    /// The AI's words as a recorded frame left them (see [`World::cpu_from_dump`]).
    pub fn from_dump(ds: &[u8], player: usize) -> Cpu {
        World::cpu_from_dump(ds, player)
    }

    fn clear_intents(&mut self) {
        self.intent = [0; 6];
    }

    // ---- the driver ---------------------------------------------------------------------

    /// One frame of the original's `CS:129c`. Returns the key flags the keyboard would have given
    /// the spy, plus the direct writes.
    pub fn think(&mut self, w: &World, rng: &mut dyn Drawer) -> (Keys, Side) {
        let me = self.player;
        let mut k = Keys::default();
        let mut side = Side::default();
        // `CS:12cc`: a spy who is animating or out of the round gets no input at all.
        if w.anim[me] || w.frozen[me] != 0 {
            return (k, side);
        }
        if self.cooldown > 0 {
            self.cooldown -= 1;
        }
        // `CS:12e8`: all six intents zero -> decide now; otherwise count down and decide at zero.
        if self.intent.iter().any(|&v| v != 0) {
            self.countdown -= 1;
            if self.countdown < 1 {
                self.clear_intents();
                self.decide(w, rng, &mut side);
            }
        } else {
            self.clear_intents();
            self.decide(w, rng, &mut side);
        }
        // `CS:1336..13ff` — the three hooks that run while the spy is walking.
        if self.intent[I_NAV] != 0 {
            self.leave_the_walk(w, &mut side);
            if self.intent[I_NAV] != 0 {
                self.look_for_item(w, rng);
            }
            if self.intent[I_NAV] != 0 {
                self.fidget(w, rng);
            }
        }
        // `CS:1401..1449` — the dispatch, in the original's order.
        if self.intent[I_MAP] != 0 {
            self.beh_map(w, &mut k, &mut side);
        } else if self.intent[I_ITEM] != 0 {
            self.beh_item(w, &mut k, &mut side);
        } else if self.intent[I_GRAB] != 0 {
            self.beh_grab(w, &mut k, &mut side);
        } else if self.intent[I_NAV] != 0 {
            self.beh_nav(w, &mut k, rng);
        } else if self.intent[I_ATTACK] != 0 {
            let (akk, _s) = self.attack(w);
            k = akk;
        } else if self.intent[I_WARM] != 0 {
            self.beh_warm(w, &mut k, rng);
        }
        // **A stray panel.** The original's AI opens the trapulator only from the map behaviour
        // (`CS:1b10`) and the item behaviour (`CS:1beb`), and closes it from `CS:2a4e` when A lands
        // on a slot that holds something or on the map (`CS:2a87`). It can still get one open the
        // *other* way: two of its own A presses at an empty cell are the player's two-tap gesture
        // (`CS:2d50`), and once the panel is open nothing in `CS:129c` closes it — the spy then
        // stands in the panel for the rest of the round. Measured in this port's own rounds before
        // the guard (island 7, frame 5: panel open, 4,139 frames without a step). Port decision:
        // when no behaviour of the AI's wants the panel, close it.
        if w.trap_open[me] && self.intent[I_MAP] == 0 && self.intent[I_ITEM] == 0 {
            side.trap_open = Some(false);
        }
        (k, side)
    }

    /// `CS:1ca9` — **the goal selector**, run every tenth frame (or the moment a behaviour ends).
    ///
    /// ```text
    /// 1ca9  count = popcount(0x2EB8 & {0x40,0x80,0x200})        ; the three parts in the case
    ///       bx = 0x30                                           ; both spies on one screen?
    ///          + 0x60                                          ; the clock at or below 60
    ///          + (black's hand holds a part ? 1 or count : 0)
    ///          + 4*(white's hand holds a part ? 1 or count : 0)
    ///          + 0x10*(count-1)
    ///       ax = byte[0x30AC + bx], clamped to 5
    ///       ax = 4 if (I am in water/climb and ax <= 1)
    ///       ax = warm/exit if my cold word >= 0x31CB[difficulty]
    /// ```
    /// The two hand terms are **the colours'**, not mine/theirs: `0x2F2E` is the black spy's hand
    /// and `0x2F2C` the white's, weighted 1 and 4. That is faithful for the only case the original
    /// has (black is the computer) and is kept colour-fixed for `--cpu1`.
    pub fn decide(&mut self, w: &World, _rng: &mut dyn Drawer, side: &mut Side) {
        let me = self.player;
        let d = self.difficulty.min(4) as usize;
        let parts = [0x40u16, 0x80, 0x200]
            .iter()
            .filter(|&&b| w.mask & b != 0)
            .count() as i32;
        let shared = w.visitors[0] + w.visitors[1] != 0;
        let mut bx: i32 = 0;
        // `CS:1cd6`: the bonus needs a shared screen, the other spy alive, nobody sunk, and me not
        // in one of the five "busy" animation states.
        if shared
            && w.frozen[1 - me] == 0
            && w.sink[0] + w.sink[1] == 0
            && ![0x0e, 0x12, 0x1a, 0x10, 0x20].contains(&w.pose_state[me])
        {
            bx = 0x30;
        } else {
            self.can_act = 0;
        }
        if w.clock_left <= 0x3c {
            bx += 0x60;
        }
        // `0x2F2E` is the black spy's hand and `0x2F2C` the white's; the original only drives
        // black, so they read as "mine" and "his" - and do so for the port's white computer too.
        let black = w.carried[me];
        let white = w.carried[1 - me];
        if black >= 9 {
            bx += if black >= 0x0c { parts } else { 1 };
        }
        if white >= 9 {
            bx += 4 * if white >= 0x0c { parts } else { 1 };
        }
        if parts - 1 > 0 {
            bx += 0x10 * (parts - 1);
        }
        let mut ax = w.actions[(bx as usize).min(0xBF)].min(5) as u16;
        // `CS:1d7a`: a spy in water or climbing cannot fight; he walks.
        if (w.ground[me] == 1 || w.ground[me] == 0x17) && ax <= 1 {
            ax = ACT_CHASE;
        }
        if w.cold[d] <= w.heat[me] as u16 {
            ax = if white == 0x0c || parts < 2 || !w.case_owned[me] || black == 0x0c {
                ACT_WARM
            } else {
                ACT_EXIT
            };
        }
        self.action = ax;
        match ax {
            ACT_WARM => self.intent[I_WARM] = 1,
            0 | 1 => {
                side.trap_open = Some(false);
                self.intent[I_ATTACK] = 1;
            }
            _ => self.intent[I_NAV] = 1,
        }
        self.countdown = 10;
    }

    /// `CS:1559` — **anything in my hand**(`0x2F2E` not -1 and not the briefcase) abandons the
    /// walk and arms the put-away latch, which is how the computer clears its hands before setting
    /// off. (Stage 15's note read this word as `0x2FAA`; the disassembly is `0x2f2e`.)
    fn leave_the_walk(&mut self, w: &World, side: &mut Side) {
        let c = w.carried[self.player];
        if c != -1 && c != 0x0c {
            self.intent[I_NAV] = 0;
            side.act_latch = Some(true);
        }
    }

    /// `CS:144d` — **what is under my feet.** Three arms, and the *buried* case matters:
    ///
    /// ```text
    /// 145b  code = byte[0x32D9] ; if 0 return
    /// 1467  if (code & 0x60) goto 1510            ; a planted stick (the owner's tag is in bits 5/6)
    /// 1475  if (code & 0x80) goto SET_GRAB        ; buried: press A to dig it up
    /// 147a  bit = word[0x3176 + 2*code]
    /// 1484  if (bit < 0) goto SET_GRAB            ; 0xFFFF = the briefcase
    /// 1489  if (bit == 0) goto 14a3               ; the dynamite (and code 8's own arm)
    /// 148b  if (code == 8) { 0x3311 >= 0xF ? return : SET_GRAB }
    /// 1492  if (bit & kit) return                 ; already have that tool
    ///       SET_GRAB: 0x2EDC = 1; nav = 0; countdown = 10
    /// ```
    /// There is **no random test** on this path: an item the spy does not have is always worth a
    /// press. The draw appears only in the stick arm and in the dynamite arm.
    fn look_for_item(&mut self, w: &World, rng: &mut dyn Drawer) {
        let me = self.player;
        if self.can_act == 1 {
            return;
        }
        let code = w.cell[me] as u16;
        if code == 0 {
            return;
        }
        self.found = code;
        let d = self.difficulty.min(4) as usize;
        let th = w.thresholds[d];
        if code & 0x60 != 0 {
            // a stick somebody planted: the low five bits are the item, bits 5/6 the owner's tag
            if (code & 0x1f) != 0x0c && !((code & 0x1f) >= 9 && w.carried[me] == 0x0c) {
                self.intent[I_GRAB] = 0;
                if code & w.tag[me] as u16 != 0 {
                    return;
                }
                if rng.draw() < th {
                    return;
                }
            }
            self.set_grab();
            return;
        }
        if code & 0x80 != 0 {
            self.set_grab(); // a mound: press A and dig
            return;
        }
        let bit = if code <= 0x0c { w.code_bits[code as usize] } else { 0 };
        if (bit as i16) < 0 {
            self.set_grab(); // the briefcase
            return;
        }
        if bit == 0 {
            // `CS:14a3`: an item with no kit bit — a bare part (9..11), or code 0/1. A part is
            // taken **only with the briefcase in hand** (it goes straight into the case); with a
            // free hand and a stick in the kit the computer may instead *trap* it with dynamite.
            if code >= 9 && w.carried[me] == 0x0c {
                self.set_grab();
                return;
            }
            if w.carried[me] != -1 {
                return;
            }
            if w.kit[me] & 0x20 == 0 {
                return;
            }
            if code > 0x0c {
                return;
            }
            if code >= 9 && w.case_owned[me] {
                return;
            }
            if rng.draw() < th {
                return;
            }
            self.intent[I_NAV] = 0;
            self.intent[I_ITEM] = 1;
            self.target_slot = 0x0a;
            self.slot_confirm = 1;
            self.countdown = 10;
            return;
        } else if code == 8 {
            // `CS:148b`: the loose stick — taken while the trapulator holds fewer than fifteen
            if w.dyn_sticks[me] >= 0x0f {
                return;
            }
        } else if bit & w.kit[me] != 0 {
            return; // already in my kit
        }
        self.set_grab();
    }

    /// `CS:1546` — `0x2EDC = 1`, the walk abandoned, the countdown reloaded.
    fn set_grab(&mut self) {
        self.intent[I_GRAB] = 1;
        self.intent[I_NAV] = 0;
        self.countdown = 10;
    }

    /// `CS:1366` — the idle fidget. While walking, two draws gate it: one above 0x77, then one at
    /// or above the difficulty threshold. Either the trapulator's map comes out, or a tool leaves
    /// the kit (bucket / saw / dynamite, in that order of preference).
    fn fidget(&mut self, w: &World, rng: &mut dyn Drawer) {
        let me = self.player;
        let d = self.difficulty.min(4) as usize;
        if self.cooldown != 0 {
            return;
        }
        if rng.draw() <= 0x77 {
            return;
        }
        if rng.draw() < w.thresholds[d] {
            return;
        }
        if w.sink[me] != 0 || w.visitors[0] + w.visitors[1] != 0 {
            return;
        }
        self.intent[I_NAV] = 0;
        if rng.draw() < 0x50 {
            self.intent[I_MAP] = 1;
            return;
        }
        if w.frozen[1 - me] == 0 {
            self.intent[I_ITEM] = 1;
            let b = rng.draw();
            let kit = w.kit[me];
            if b < 0x50 {
                if kit & 4 != 0 {
                    self.target_slot = 4;
                    return;
                }
            } else if b < 0xa0 {
                if kit & 2 != 0 {
                    self.target_slot = 2;
                    return;
                }
            } else if kit & 0x20 != 0 {
                self.target_slot = 0x0a;
                return;
            }
        }
        self.intent[I_ITEM] = 0;
        self.intent[I_NAV] = 1;
    }

    // ---- the six behaviours --------------------------------------------------------------

    /// `CS:1aa8` — **the map.** Stow what is in hand, step off anything under the feet, open the
    /// trapulator, select slot 12 (the map), press A, look at it for 24 frames, press A again.
    fn beh_map(&mut self, w: &World, k: &mut Keys, side: &mut Side) {
        let me = self.player;
        self.countdown = 10;
        let st = self.intent[I_MAP];
        if st == 1 {
            if w.carried[me] != -1 {
                // `FUN_26c0_315f`, then `CS:1ad0`: a hand that is still full ends the behaviour.
                side.stow = true;
                if !w.stow_ok[me] {
                    self.map_done();
                    return;
                }
                self.intent[I_MAP] = 2;
                return;
            }
        } else if st <= 4 {
            // fall through to the step
        } else if st == 5 {
            self.map_panel(w, k, side);
            return;
        } else {
            self.intent[I_MAP] += 1;
            if self.intent[I_MAP] < 0x1e {
                return;
            }
            k.fire = true;
            self.map_done();
            return;
        }
        self.intent[I_MAP] += 1;
        if w.cell[me] != 0 {
            if self.intent[I_MAP] == 5 {
                self.map_panel(w, k, side);
                return;
            }
            k.right = true;
            return;
        }
        self.map_panel(w, k, side);
    }

    fn map_panel(&mut self, w: &World, k: &mut Keys, side: &mut Side) {
        let me = self.player;
        self.intent[I_MAP] = 5;
        if !w.trap_open[me] {
            side.trap_open = Some(true);
        }
        // stage 20c: the map's errand opens on slot 12 (`CS:1b19` compares against `0x0C`)
        if w.trap_cursor[me] != 0x0c {
            side.trap_cursor = Some(0x0c);
            return;
        }
        if w.trap_cursor[me] == 0x0c {
            k.fire = true;
            self.intent[I_MAP] += 1;
        } else {
            k.right = true;
        }
    }

    fn map_done(&mut self) {
        self.intent[I_MAP] = 0;
        self.cooldown = 0x14;
    }

    /// `CS:1b5a` — **use a tool.** Stow the hand, walk off whatever is under the feet, open the
    /// trapulator, step the cursor to the slot the selector asked for (`DS:0x2F98`, the black
    /// spy's half of the pair `0x2F96 + si`), press A, then press A again once the panel has run.
    ///
    /// ```text
    /// 1b93  if (my hand == -1) goto 1bb0        ; free hand: no stow, straight to the wait arm
    ///       call 0x315f ; if (my hand != -1) goto 1c3a ; 0x2ED4 = 2 ; return
    /// 1bb0  if (0x2F9C == 1) goto 1bde          ; the slot is already chosen
    ///       ++0x2ED4 ; if (nothing under the feet) goto 1bde
    ///       if (0x2ED4 == 5) goto 1c3a ; press RIGHT ; return
    /// 1bde  0x2ED4 = 0x0B ; open the panel ; cursor == 0x2F98 ? press A, ++ : press RIGHT
    /// 1c2b  tail (states 0x0F+): cmpw $0xa,0x2f96(%si) ; equal -> no press ; else press A
    ///       then 0x2ED4 = 0, 0x2F9C = 0, 0x2F94 = 0x14
    /// ```
    fn beh_item(&mut self, w: &World, k: &mut Keys, side: &mut Side) {
        let me = self.player;
        self.countdown = 10;
        let st = self.intent[I_ITEM];
        if st == 1 {
            if w.carried[me] != -1 {
                side.stow = true;
                if !w.stow_ok[me] {
                    // `CS:1c3a`
                    self.intent[I_ITEM] = 0;
                    self.slot_confirm = 0;
                    self.cooldown = 0x14;
                    return;
                }
                self.intent[I_ITEM] = 2;
                return;
            }
            // `CS:1b98 je 0x1bb0`: a **free hand** needs no stow, and the original does not fall
            // through to the `1c2b` tail from here — it goes to the wait arm, which opens the panel
            // and walks the cursor. Falling through skipped the whole panel interaction and ended
            // the errand on one stray A press (stage 18d: this killed every tool the idle fidget
            // asked for, since the fidget's tools come out of the kit with the hand empty).
            self.item_wait(w, k, side);
            return;
        }
        if st <= 4 {
            self.item_wait(w, k, side);
            return;
        } else if st <= 0x0a {
            self.intent[I_ITEM] += 1;
            return;
        } else if st == 0x0b {
            self.item_select(w, k, side);
            return;
        } else if st <= 0x0d {
            self.intent[I_ITEM] += 1;
            return;
        } else if st == 0x0e {
            // button B + up: the tool's own action, aimed
            k.fire = true;
            k.up = true;
            self.intent[I_ITEM] += 1;
            return;
        }
        if self.target_slot != 0x0a {
            k.fire = true;
        }
        self.intent[I_ITEM] = 0;
        self.slot_confirm = 0;
        self.cooldown = 0x14;
    }

    fn item_wait(&mut self, w: &World, k: &mut Keys, side: &mut Side) {
        let me = self.player;
        if self.slot_confirm == 1 {
            self.item_select(w, k, side);
            return;
        }
        self.intent[I_ITEM] += 1;
        if w.cell[me] != 0 {
            if self.intent[I_ITEM] == 5 {
                self.intent[I_ITEM] = 0;
                self.slot_confirm = 0;
                self.cooldown = 0x14;
                return;
            }
            k.right = true;
            return;
        }
        self.item_select(w, k, side);
    }

    /// **stage 20c — the panel opens on the slot that was asked for.**
    ///
    /// The original walks the cursor here (`CS:1bf1` compares `0x2F3E` with `0x2F98`, and a
    /// mismatch presses RIGHT at `CS:1c09`), and the panel's repeat counter makes that three frames
    /// a slot (`DS:0x2F9E`: `CS:2af6` decrements it, `CS:2b04` reloads 2). The recording does not
    /// show the walk: every panel frame of the d1/d2 rounds carries the cursor on the requested slot
    /// and the panel is up for one dump, which a walk of one to five slots (3–15 frames) cannot
    /// produce. `DS:0x2F3C` is a word the game writes outside the panel too — `FUN_26c0_315f`
    /// stores the stow table's slot into it (`CS:31aa`) — and `DS:0x2F9C` is the "the slot is
    /// already chosen" arm (`CS:1bb0` goes straight to `CS:1bde`), so the errand places the word
    /// and presses A on the next frame. That is the variant the recorded frames support; the code
    /// read alone cannot settle which writer does it (`STAGE20C_REPORT.md` §1).
    fn item_select(&mut self, w: &World, k: &mut Keys, side: &mut Side) {
        let me = self.player;
        self.intent[I_ITEM] = 0x0b;
        if !w.trap_open[me] {
            side.trap_open = Some(true);
        }
        // the panel is up on the slot the errand asked for — including a panel another errand
        // left open, which is how a map look that the countdown ended mid-way hands over a panel
        // sitting on slot 12. One frame to place it, the next to press A.
        if w.trap_cursor[me] != self.target_slot && self.target_slot != 0 {
            side.trap_cursor = Some(self.target_slot);
            return;
        }
        if w.trap_cursor[me] == self.target_slot {
            k.fire = true;
            self.intent[I_ITEM] += 1;
        } else {
            k.right = true;
        }
    }

    /// `CS:1c4d` — **grab.** Arm the put-away latch if a tool is in hand, then press A on the frame
    /// the hand is free.
    fn beh_grab(&mut self, w: &World, k: &mut Keys, side: &mut Side) {
        let me = self.player;
        if self.intent[I_GRAB] < 2 {
            self.intent[I_GRAB] = 2;
            if w.carried[me] >= 0 && w.carried[me] != 0x0c {
                side.act_latch = Some(true);
                return;
            }
        }
        if self.intent[I_GRAB] < 3 {
            self.intent[I_GRAB] = 3;
            if w.carried[me] == -1 || w.carried[me] == 0x0c {
                k.fire = true;
                return;
            }
        } else {
            self.intent[I_GRAB] += 1;
            if self.intent[I_GRAB] < 5 {
                return;
            }
        }
        self.intent[I_GRAB] = 0;
    }

    /// `CS:1574` — **head for something.** Targets come from the position record the round start
    /// keeps (the hatch, the parts, the case, see [`Link`]), or the other spy:
    ///
    /// ```text
    /// 1574  if (action == 4) goto CHASE
    /// 157e  if (action <= 3) { bp = 0; goto GO }                  ; the hatch
    /// 1585  if (black's hand == case) { for bp in 6,4,2: if link[bp].screen == my screen goto GO_CELL
    ///                                   bp = 6; goto 15be }
    /// 158c  if (white's hand == case) goto CHASE
    /// 1593  bp = 8; goto GO                                       ; the case, wherever it was recorded
    /// 15be  loop bp = 6,4,2,0: if (!(my kit & word[0x3237+bp]) && link[bp].screen >= 0) break
    ///       if (bp == 0) goto CHASE else GO
    /// ```
    /// Note the test at `15c2` is on **my kit word** (`0x32A3`), not the deposited-parts ledger.
    fn beh_nav(&mut self, w: &World, k: &mut Keys, rng: &mut dyn Drawer) {
        let me = self.player;
        let links = w.links;
        let chase = |s: &mut Cpu, k: &mut Keys, rng: &mut dyn Drawer| {
            *k = s.chase(w, rng);
        };
        let bp: usize = match self.action {
            ACT_CHASE => {
                chase(self, k, rng);
                return;
            }
            a if a <= 3 => 0,
            _ => {
                if w.carried[me] == 0x0c {
                    // `CS:1599`: a part lying in my own room first
                    let mut hit = None;
                    for bp in [3usize, 2, 1] {
                        if links[bp].screen == w.screen[me] {
                            hit = Some(bp);
                            break;
                        }
                    }
                    match hit {
                        Some(bp) => bp,
                        None => {
                            // `CS:15bb`: the first part my kit does not hold and that is on the floor
                            let mut found = None;
                            for bp in [3usize, 2, 1, 0] {
                                if w.kit[me] & PART_BIT[bp] == 0 && links[bp].used {
                                    found = Some(bp);
                                    break;
                                }
                            }
                            match found {
                                Some(0) | None => {
                                    chase(self, k, rng);
                                    return;
                                }
                                Some(bp) => bp,
                            }
                        }
                    }
                } else if w.carried[1 - me] == 0x0c {
                    chase(self, k, rng); // `CS:158c`: the white spy has the case
                    return;
                } else {
                    4 // `CS:1593`: the case, where it was last recorded
                }
            }
        };
        // `CS:15e4`: GO — row, entry and screen of the record (an invalid record is screen -1,
        // which the door route reads as a room to the west).
        let link = links[bp];
        let mut col = link.col as i16;
        if bp == 0 && w.carried[me] != 0x0c {
            col += 4; // `CS:15f3`: four entries to the side of the hatch unless the case is in hand
        }
        *k = self.navigate_r(w, link.screen, col, link.row as i16, rng);
    }

    /// `CS:1608` — walk to the other spy. If he is indoors (screen >= 10) the target is the
    /// outdoor screen of that igloo (`screen - 10`); standing on `0x30` at the igloo's door with
    /// him inside, the walk ends (`CS:162e` -> `CS:164b`, no obstacle pass).
    fn chase(&mut self, w: &World, rng: &mut dyn Drawer) -> Keys {
        let me = self.player;
        let o = 1 - me;
        let mut dx = w.screen[o];
        if dx >= 0x0a {
            dx -= 0x0a;
            if w.screen[me] + 0x0a == w.screen[o] && w.ground[me] == 0x30 {
                return Keys::default();
            }
        }
        self.navigate_r(w, dx, w.cell_col[o] + 3, w.cell_row[o], rng)
    }

    /// `CS:1886` — **the igloo.** Find the igloo whose interior row carries the 0x16 door marker,
    /// walk to its door, go in (0x1b -> down, 0x30 -> right, 0x1c -> the next state), then walk to
    /// the fire and warm up.
    fn beh_warm(&mut self, w: &World, k: &mut Keys, rng: &mut dyn Drawer) {
        let me = self.player;
        self.countdown = 10;
        let st = self.intent[I_WARM];
        if st == 1 {
            if w.cell[me] == 0x0c {
                // the briefcase lying at my feet comes first
                self.intent[I_GRAB] = 1;
                self.intent[I_WARM] = 0;
                return;
            }
            let row = w.screen[me] / 2;
            let mut found: Option<(usize, usize)> = None;
            if (row as usize) < 5 {
                for c in 0..12 {
                    if w.interiors[row as usize][c] == 0x16 {
                        found = Some((row as usize, c));
                    }
                }
            }
            // `CS:18d2`: go in unless the other spy is already inside this igloo (there is one
            // fire, so he has it); `0x2FB0 == 1` means the island lit only this one.
            let other_inside = |igloo_of: u16| {
                w.screen[1 - me] >= 0x0a && w.screen[1 - me] - 0x0a == igloo_of
            };
            let at_igloo = found.is_some() && (w.fires == 1 || !other_inside(w.screen[me]));
            if let (true, Some((_r, c))) = (at_igloo, found) {
                self.igloo_door(w, k, rng, c);
                return;
            }
            // `CS:18f6`: any igloo at all, from screen 8 down to 0
            let mut s = 8i32;
            loop {
                if s < 0 {
                    break;
                }
                let base = s as usize / 2;
                if base < 5 {
                    for c in 0..12 {
                        if w.interiors[base][c] == 0x16 {
                            if w.fires == 1 || !other_inside(s as u16) {
                                *k = self.navigate_r(w, s as u16, 0, 0, rng);
                                return;
                            }
                        }
                    }
                }
                s -= 2;
            }
            return;
        }
        if st <= 2 {
            if w.ground[me] == 0x1d {
                self.intent[I_WARM] = 3;
            } else {
                k.left = true;
            }
            return;
        }
        if st <= 3 {
            if w.heat[me] <= 3 {
                self.intent[I_WARM] = 4;
            } else {
                k.up = true;
            }
            return;
        }
        // state 4: inside, head for the fire
        match w.ground[me] {
            0 => self.intent[I_WARM] = 0,
            0x1b => k.down = true,
            _ => k.right = true,
        }
    }

    /// `CS:1945` + `CS:1977` — at the igloo: walk to its door column (`gate + 11`), or the three
    /// interior gestures that get the spy through it.
    fn igloo_door(&mut self, w: &World, k: &mut Keys, rng: &mut dyn Drawer, col: usize) {
        let me = self.player;
        match w.ground[me] {
            0x1c => {
                self.intent[I_WARM] = 2;
                return;
            }
            0x1b => {
                k.down = true;
                return;
            }
            0x30 => {
                k.right = true;
                return;
            }
            _ => {}
        }
        let target = (w.gate[col] >> 1) as i16 + 0x0b;
        *k = self.navigate_r(w, w.screen[me], target, 0, rng);
    }

    /// `CS:1a01` — **the punch.** Stow what is in hand, close the distance, then press B and walk
    /// into the other spy. Deep snow and ice make him slide instead, so there the routine only
    /// presses left.
    pub fn attack(&mut self, w: &World) -> (Keys, Side) {
        let me = self.player;
        let o = 1 - me;
        let mut k = Keys::default();
        let mut side = Side::default();
        self.countdown = 1;
        if w.carried[me] >= 0 {
            side.stow = true;
        }
        let dy = (w.y[o] - w.y[me]).abs();
        if dy > 4 {
            if w.y[o] < w.y[me] {
                if w.ground[me] == 0x30 {
                    if w.x[me] < w.x[o] {
                        k.right = true
                    } else {
                        k.left = true
                    }
                } else {
                    k.up = true;
                }
            } else {
                k.down = true;
            }
            self.intent[I_ATTACK] = 0;
            return (k, side);
        }
        if w.ground[me] == 2 || w.ground[me] == 3 {
            k.left = true;
            return (k, side);
        }
        // **button B, not A** (`CS:1a46` sets `0x3283`): the frame body tests B as a level, so a
        // punch is re-pressed on every frame the behaviour runs (stage 18f). Writing `k.fire`
        // here would be button A, whose flag is consumed by acting — the AI would bury and re-dig
        // whatever it was carrying instead of throwing.
        k.fire_b = true;
        if w.x[me] > w.x[o] {
            k.left = true;
        } else {
            k.right = true;
        }
        (k, side)
    }

    // ---- navigation ----------------------------------------------------------------------

    /// `CS:164c` + `CS:1774` — steer towards `(screen, col, row)`, with a fixed stream.
    ///
    /// `#[cfg(test)]`: the engine always calls [`Self::navigate_r`] with the round's own generator,
    /// because the original's nav avoidance draws from `CS:20CD` like everything else — the fixed
    /// seed this wrapper uses is a test convenience (a route that does not depend on the round's
    /// draw order), so it is compiled for the tests and not shipped.
    #[cfg(test)]
    pub fn navigate(&mut self, w: &World, screen: u16, col: i16, row: i16) -> Keys {
        let mut r = Rng::new(0x5EED_0017);
        self.navigate_r(w, screen, col, row, &mut r)
    }

    /// The same, against a caller's stream: the original's nav avoidance draws from `CS:20CD`, so
    /// `navigate_r` is what the engine and the AI use.
    pub fn navigate_r(
        &mut self,
        w: &World,
        screen: u16,
        col: i16,
        row: i16,
        rng: &mut dyn Drawer,
    ) -> Keys {
        let mut k = self.door_route(w, screen, col, row);
        self.avoid(w, &mut k, rng);
        k
    }

    /// `CS:164c` — the door route. Same room: stay inside the y band, then line the column up, then
    /// the row. Another room: find the door in my own room that leads that way and walk to it.
    fn door_route(&mut self, w: &World, screen: u16, col: i16, row: i16) -> Keys {
        let me = self.player;
        let mut k = Keys::default();
        // `CS:164c`'s head: water and the climb cell send him up.
        if w.ground[me] == 0x16 || w.ground[me] == 0x17 {
            k.up = true;
            return k;
        }
        if screen == w.screen[me] {
            if w.y[me] >= 0x3a {
                k.up = true;
                return k;
            }
            if w.y[me] < 0x26 {
                k.down = true;
                return k;
            }
            if w.cell_col[me] < col {
                k.right = true;
                return k;
            }
            if col < w.cell_col[me] {
                k.left = true;
                return k;
            }
            if w.cell_row[me] < row {
                k.down = true;
                return k;
            }
            if w.cell_row[me] > row {
                k.up = true;
                return k;
            }
            return k;
        }
        let row_of = |s: u16| -> usize { (s / 2) as usize };
        let base = row_of(w.screen[me]);
        if (screen as i16) < (w.screen[me] as i16) {
            // west: the room below is reached through a **down** door (level 10)
            if w.y[me] > 0x25 {
                if let Some(x) = door_x(w, base, &[10]) {
                    if x < w.x[me] {
                        k.left = true;
                        return k;
                    }
                    if x != w.x[me] {
                        k.right = true;
                        return k;
                    }
                }
            }
            k.down = true;
            return k;
        }
        // east: the room above is reached through an **up** door (level 2 or 24)
        if w.y[me] < 0x3a {
            if let Some(x) = door_x(w, base, &[2, 0x18]) {
                if x < w.x[me] {
                    k.left = true;
                    return k;
                }
                if x != w.x[me] {
                    k.right = true;
                    return k;
                }
            }
        }
        k.up = true;
        k
    }

    /// `CS:1774` — **the obstacle pass.** Runs after the route and may overrule it: a wall ahead
    /// makes the spy side-step (up when he is low in the band, down when he is high), the hatch's
    /// own bytes make him press A, scenery codes make him back out, and a hard bump arms the retry
    /// timer so he stops hammering the wall.
    fn avoid(&mut self, w: &World, k: &mut Keys, rng: &mut dyn Drawer) {
        let me = self.player;
        let d = self.difficulty.min(4) as usize;
        let threshold = w.thresholds[d];
        let mut wall = w.blocked[me];
        if !wall {
            match w.ground[me] {
                0x1b | 0x1c | 0x30 => {
                    k.left = false;
                    k.right = false;
                    k.up = false;
                    k.down = true;
                    self.retry = self.retry.saturating_sub(1);
                    return;
                }
                0x31 | 0x33 | 0x32 | 0x34 => {
                    // `CS:1794..17bd`: on the plates' edge codes, pressing towards the edge (left
                    // for 0x31/0x33, right for 0x32/0x34) draws a byte; below the difficulty
                    // threshold the response is the scenery one (`CS:1791` -> `CS:1862`): left and
                    // right cleared, DOWN. At or above it the pass carries on to the object-byte test.
                    let towards = if matches!(w.ground[me], 0x31 | 0x33) { k.left } else { k.right };
                    if towards && rng.draw() < threshold {
                        k.left = false;
                        k.right = false;
                        k.down = true;
                        self.retry = self.retry.saturating_sub(1);
                        return;
                    }
                    self.object_ahead(w, k, rng, &mut wall);
                }
_ => self.object_ahead(w, k, rng, &mut wall),
            }
        }
        if wall {
            k.left = false;
            k.right = false;
            k.up = false;
            k.down = false;
            if w.y[me] > 0x2b {
                k.up = true;
            } else {
                k.down = true;
            }
        }
        self.retry = self.retry.saturating_sub(1);
    }
}

impl Cpu {
    /// `CS:17cd..182b` — the object byte one entry ahead (in the direction of the pressed
    /// left/right key). The hatch's own bytes (0x0F, 0x15) are a wall unless the goal is the hatch
    /// (action 3) or a draw above 0xF0; any other object byte of 0x0D or more is a wall unless
    /// the retry timer is past 0x3C or a draw clears the difficulty threshold (a wall hit arms
    /// the timer: +5). **Terrain is not looked at** - see the stage 17b notes.
    fn object_ahead(&mut self, w: &World, k: &Keys, rng: &mut dyn Drawer, wall: &mut bool) {
        let me = self.player;
        let threshold = w.thresholds[self.difficulty.min(4) as usize];
        let dir = if k.left {
            Some(w.ahead_w[me])
        } else if k.right {
            Some(w.ahead_e[me])
        } else {
            None
        };
        if let Some(b) = dir {
            let b = b & 0x1f;
            if b == 0x0f || b == 0x15 {
                // the hatch's three bytes: `CS:1814`
                if !(self.action == 3 || rng.draw() > 0xf0) {
                    *wall = true;
                }
            } else if b >= 0x0d && !(self.retry > 0x3c || rng.draw() > threshold) {
                self.retry = self.retry.saturating_add(5);
                *wall = true;
            }
        }
    }
}

/// `DS:0x3237` — the kit bit of each link: entry 1..3 are the three parts, entry 0 the case.
const PART_BIT: [u16; 5] = [0x1000, 0x40, 0x80, 0x200, 0x400];

/// The world x of the first door in `row` whose level code is one of `codes` (`CS:1709`/`174a`
/// walk the twelve columns from the right, and the door's x is `gate + 0x0b`).
fn door_x(w: &World, row: usize, codes: &[u16]) -> Option<i32> {
    if row >= w.levels.len() {
        return None;
    }
    for c in (0..12).rev() {
        if codes.contains(&w.levels[row][c]) {
            return Some(w.gate[c] as i32 + 0x0b);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{RoundOpts};
    use std::path::PathBuf;

    /// A fixed draw sequence, so a behaviour can be pushed into one arm. The last value repeats.
    struct Seq(Vec<u16>, usize);
    impl Seq {
        fn of(v: &[u16]) -> Seq {
            Seq(v.to_vec(), 0)
        }
    }
    impl Drawer for Seq {
        fn draw(&mut self) -> u16 {
            let v = self.0[self.1.min(self.0.len() - 1)];
            self.1 += 1;
            v
        }
    }

    fn exe_path() -> PathBuf {
        crate::orig::require("spy-vs-spy-iii/original/256/spy.exe")
    }
    fn exe() -> Vec<u8> {
        std::fs::read(exe_path()).expect("spy.exe")
    }
    fn game(island: usize, players: usize) -> Game {
        Game::new(exe(), &RoundOpts { island, players, ..RoundOpts::default() }).expect("round")
    }

    /// A synthetic 64 KB data segment holding just the words a test cares about, so the dump
    /// readers (`World::from_dump`, `Cpu::from_dump`) can be asked about one address without a
    /// recording. Everything else is 0, which is what the round-start template holds
    /// (`CS:125f` restores `0x2EB8..0x2FB1` from `0x2FB2`).
    fn dump(words: &[(usize, u16)]) -> Vec<u8> {
        let mut ds = vec![0u8; 0x10000];
        for &(a, v) in words {
            ds[a] = (v & 0xff) as u8;
            ds[a + 1] = (v >> 8) as u8;
        }
        ds
    }

    // ---- the tables are the executable's own records -------------------------------------

    #[test]
    fn the_difficulty_tables_are_the_executables_own() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let ds = &exe[crate::game::DS_FILE_OFF..];
        let w = |a: usize| u16::from_le_bytes([ds[a], ds[a + 1]]);
        for k in 0..5 {
            assert_eq!(DIFF_THRESHOLDS[k], w(0x316C + 2 * k), "0x316C[{k}]");
            assert_eq!(DIFF_COLD[k], w(0x31CB + 2 * k), "0x31CB[{k}]");
            assert_eq!(DIFF_ART[k], w(0x31E1 + 2 * k), "0x31E1[{k}]");
        }
        assert_eq!(DIFF_THRESHOLDS, [180, 205, 225, 250, 254]);
        assert_eq!(DIFF_COLD, [42, 40, 38, 36, 34]);
        // `CS:1066` steps `0x31BF` by two, and `CS:1d90` reads a *word* at `0x31CB + 0x31BF`,
        // so difficulty d (0..4) reads word entry d — the same index the constants above use.
        for d in 0..5usize {
            let word = w(0x31CB + 2 * d) as usize;
            assert_eq!(word, DIFF_COLD[d] as usize);
        }
    }

    #[test]
    fn the_action_table_is_the_executables_own() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let ds = &exe[crate::game::DS_FILE_OFF..];
        let g = game(0, 2);
        let w = World::from_game(&g, 1);
        assert_eq!(&w.actions[..], &ds[0x30AC..0x316C], "the 192 bytes at 0x30AC");
        assert_eq!(w.actions[0], 0x05, "band 0, no parts, no invite");
        assert_eq!(w.actions[0x10], 0x05, "band 0 + one part row");
        assert_eq!(w.actions[0x60], 0x05, "the last-minute band, apart");
        assert!(w.actions.iter().all(|&b| b <= 9), "no entry above 9");
    }

    // ---- the goal selector (`CS:1ca9`) ------------------------------------------------------

    /// The selector's index arithmetic, against the table's own bytes:
    ///
    /// ```text
    ///  bx = 0x30 (the two spies share a screen and can act)
    ///     + 0x60 (the clock is at or below 60 counts)
    ///     + 1 or parts (my hand holds a part)
    ///     + 4 or 4*parts (the other spy's hand does)
    ///     + 0x10*(parts-1)
    ///  ax = byte[0x30AC + bx] ; clamps to 5 ; 0/1 = attack, 2..5 = walk, 6 = warm
    /// ```
    fn decide(c: &mut Cpu, w: &World) {
        c.clear_intents();
        let mut side = Side::default();
        c.decide(w, &mut Seq::of(&[0x00; 8]), &mut side);
    }

    #[test]
    fn the_selector_reads_the_action_table() {
        // Apart (no visitor flag): band 0, byte 0 = 0x05 = "walk to the exit door".
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &World::default());
        assert_eq!(c.action, ACT_EXIT);
        // Sharing a screen: +0x30, byte 0x30 = 0x01 = attack. That is the computer turning on the
        // other spy as soon as it can see him — the table's whole 0x30 band is 0/1.
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &World { visitors: [1, 0], ..World::default() });
        assert_eq!(c.action, 1, "byte 0x30 is 1, which is the attack code as much as 0 is");
        assert_eq!(c.intent[I_ATTACK], 1);
        // The last minute *and* apart: band 0x60, byte 0x60 = 0x05 — still the exit, not a fight.
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &World { clock_left: 60, ..World::default() });
        assert_eq!(c.action, ACT_EXIT);
        // Two parts deposited but apart: +0x10 -> byte 0x10 = 0x05.
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &World { mask: 0x400 | 0x40 | 0x80, ..World::default() });
        assert_eq!(c.action, ACT_EXIT);
        // Two parts deposited *and* sharing: 0x30 + 0x10 = 0x40 -> byte 0 = attack.
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &World { mask: 0x400 | 0x40 | 0x80, visitors: [1, 0], ..World::default() });
        assert_eq!(c.action, ACT_ATTACK, "byte 0x40 is 0");
    }

    #[test]
    fn a_cold_spy_is_sent_to_the_igloo_and_a_warm_one_is_not() {
        let mut c = Cpu::new(1, 4);
        decide(&mut c, &World { heat: [3, 34], difficulty: 4, ..World::default() });
        assert_eq!(c.action, ACT_WARM, "cold at the difficulty-5 limit: warm up");
        assert_eq!(c.intent[I_WARM], 1);
        let mut c = Cpu::new(1, 4);
        decide(&mut c, &World { heat: [3, 33], difficulty: 4, ..World::default() });
        assert_ne!(c.action, ACT_WARM, "one below the limit: carry on");
    }

    #[test]
    fn the_idle_fidget_opens_the_map_on_a_low_draw() {
        let mut c = Cpu::new(1, 0);
        // The fidget block needs: the nav intent live, an empty hand, no pose state, no sink,
        // nothing in cooldown, a draw > 0x77 and a second draw >= the threshold.
        c.intent[I_NAV] = 1;
        c.countdown = 5;
        let w = World { difficulty: 4, ..World::default() };
        let mut r = Seq::of(&[0x80, 0xFF, 0x10, 0x10]);
        let (_k, _s) = c.think(&w, &mut r);
        assert_ne!(c.intent[I_MAP], 0, "a low third draw opens the map, got {:?}", c.intent);
    }

    // ---- navigation (`CS:164c`) ------------------------------------------------------------

    #[test]
    fn the_door_route_stays_inside_the_band() {
        let mut c = Cpu::new(1, 0);
        let mut w = World { y: [46, 58], ..World::default() };
        let k = c.navigate(&w, w.screen[1], 60, 2);
        assert!(k.up && !k.down, "at y = 58 the route presses up, got {k:?}");
        w.y[1] = 34;
        let k = c.navigate(&w, w.screen[1], 60, 2);
        assert!(k.down && !k.up, "at y = 34 the route presses down, got {k:?}");
    }

    #[test]
    fn the_door_route_walks_towards_the_target_column() {
        let mut c = Cpu::new(1, 0);
        let w = World { cell_col: [50, 40], cell_row: [2, 2], ..World::default() };
        let k = c.navigate(&w, w.screen[1], 60, 2);
        assert!(k.right && !k.left, "east of the target: {k:?}");
        let k = c.navigate(&w, w.screen[1], 20, 2);
        assert!(k.left && !k.right, "west of the target: {k:?}");
        // the column lines up: the row decides, row 2 < row 3 -> down (0x328b)
        let k = c.navigate(&w, w.screen[1], 40, 3);
        assert!(k.down && !k.up, "same column, target row below: {k:?}");
        let k = c.navigate(&w, w.screen[1], 40, 0);
        assert!(k.up && !k.down, "same column, target row above: {k:?}");
    }

    #[test]
    fn a_wall_ahead_makes_the_spy_side_step() {
        let mut c = Cpu::new(1, 0);
        // y = 50 > 0x2B: the 1774 wall branch presses up; 0x0D..0x3C is solid.
        let mut w = World {
            y: [46, 50],
            cell_col: [50, 40],
            ahead_e: [0, 0x10],
            ground: [0, 0],
            ..World::default()
        };
        // a draw that fails the "come back later" test, so the branch fires now
        let mut r = Seq::of(&[0x00; 8]);
        c.retry = 0;
        let k = c.navigate_r(&w, w.screen[1], 60, 2, &mut r);
        assert!(k.up, "wall to the east, y > 43 -> press up: {k:?}");
        w.y[1] = 40;
        let k = c.navigate_r(&w, w.screen[1], 60, 2, &mut r);
        assert!(k.down, "wall to the east, y < 44 -> press down: {k:?}");
    }

    // ---- the attack (`CS:1a01`) -------------------------------------------------------------

    #[test]
    fn the_attack_presses_the_second_button_next_to_the_other_spy() {
        let mut c = Cpu::new(1, 0);
        let w = World { x: [110, 100], y: [46, 46], ..World::default() };
        let (k, _s) = c.attack(&w);
        assert!(k.fire_b && !k.fire, "button B (0x3283) is the punch, got {k:?}");
        assert!(k.right && !k.left, "the other spy is east: {k:?}");
        let w = World { x: [90, 100], ..World::default() };
        let (k, _s) = c.attack(&w);
        assert!(k.left && !k.right, "the other spy is west: {k:?}");
        // out of reach vertically: close the gap first (up when he is above)
        let w = World { x: [100, 100], y: [40, 46], ..World::default() };
        let (k, _s) = c.attack(&w);
        assert!(k.up && !k.fire && !k.fire_b, "he is above: walk up, got {k:?}");
        let w = World { x: [100, 100], y: [52, 46], ..World::default() };
        let (k, _s) = c.attack(&w);
        assert!(k.down && !k.fire && !k.fire_b, "he is below: walk down, got {k:?}");
    }

    // ---- the driver (`CS:129c`) ------------------------------------------------------------

    #[test]
    fn the_ai_never_writes_a_key_for_a_spy_who_is_animating() {
        let mut c = Cpu::new(1, 0);
        let w = World { anim: [false, true], ..World::default() };
        let (k, s) = c.think(&w, &mut Seq::of(&[0xFF; 8]));
        assert!(!k.any(), "a spy mid-animation gets no keys, got {k:?}");
        assert_eq!(s, Side::default());
        let w = World { anim: [true, false], ..World::default() };
        c.intent[I_NAV] = 3;
        c.countdown = 5;
        let (k, _s) = c.think(&w, &mut Seq::of(&[0xFF; 8]));
        assert!(k.any(), "the other spy's animation does not stop the AI, got {k:?}");
    }

    // ---- the igloo search (`CS:1886`) -------------------------------------------------------

    #[test]
    fn the_igloo_search_finds_the_interior_door() {
        let mut c = Cpu::new(1, 0);
        c.intent[I_WARM] = 1;
        c.countdown = 5;
        let mut w = World::default();
        // screen 4 = table row 2; its igloo interior is row 7, and the 0x16 marker at column 3
        // is how `CS:1896` finds the door.
        w.interiors[2][3] = 0x16;
        w.levels[7][3] = 0x16;
        w.y[1] = 46;
        w.cell_col[1] = 10;
        let (k, _s) = c.think(&w, &mut Seq::of(&[0xFF; 8]));
        assert!(k.right, "the igloo door is east of the spy, got {k:?}");
    }

    // ---- the two defects the soak found and the guards that close them ---------------------

    #[test]
    fn the_ai_reads_the_live_object_cell_not_the_executable_image() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(0, 2);
        g.cpu[1] = Some(Cpu::new(1, 0));
        g.test_put(1, 100, 44, 0x02); // the saw, lying on the floor of the black spy's room
        let w = World::from_game(&g, 1);
        assert_eq!(w.cell[1], 0x02, "the live segment holds the saw");
        assert_eq!(w.code_bits[2], 1, "and the saw's kit bit is still the table's");
    }

    #[test]
    fn a_stray_trapulator_panel_is_closed() {
        let mut c = Cpu::new(1, 0);
        c.intent[I_NAV] = 1;
        c.countdown = 5;
        let w = World { trap_open: [false, true], ..World::default() };
        let (_k, s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
        assert_eq!(s.trap_open, Some(false), "a panel no behaviour asked for is closed");
        // ... but the map behaviour keeps it, because it is the one that opens it
        let mut c = Cpu::new(1, 0);
        c.intent[I_MAP] = 5;
        c.countdown = 5;
        let w = World { trap_open: [false, true], trap_cursor: [0, 4], ..World::default() };
        let (_k, s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
        assert_ne!(s.trap_open, Some(false), "the map's own panel is left alone");
    }

    #[test]
    fn the_grab_takes_a_loose_tool_off_the_floor() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = game(0, 2);
        g.cpu[1] = Some(Cpu::new(1, 0));
        g.test_put(1, 100, 44, 0x02);
        let mut got = None;
        for _ in 0..26 * 200 {
            g.tick(crate::game::Input::default());
            if g.spies[1].carried == 2 {
                got = Some(g.ticks);
                break;
            }
        }
        assert!(got.is_some(), "the computer picks the saw up; carried = {}", g.spies[1].carried);
    }

    // ---- stage 17b: the wall stall ----------------------------------------------------------

    /// `CS:25ac..25cb`: the round start records the **four** placed items (bp = 2, 4, 6, 8) in
    /// the link table, whether or not the item was buried (`CS:25d7..25df` ORs 0x80 into the byte
    /// it stores *after* recording the link). The port's layout keeps the byte with the 0x80, so
    /// the link builder must mask it. Before the fix, buried parts were not linked at all and the
    /// computer walked to a missing link 4 and stood on the spot.
    #[test]
    fn every_island_links_the_three_parts_and_the_case() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        for island in 0..7 {
            let g = game(island, 2);
            let w = World::from_game(&g, 1);
            for bp in 0..5 {
                assert!(w.links[bp].used, "island {}: link {bp} missing: {:?}", island + 1, w.links);
            }
        }
    }

    /// `CS:147a`: the item test reads **`DS:0x3176`** (the AI's own item table), not `DS:0x2D60`
    /// (the kit-bit table the stow/pickup code uses). They differ for codes 1, 4 and 9..12: the
    /// bare parts are 0 in the AI's table (a part is taken only with the case in hand) and the
    /// case is 0xFFFF.
    #[test]
    fn the_ai_reads_its_own_item_table_at_3176() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        let ds = &exe[crate::game::DS_FILE_OFF..];
        let w16 = |a: usize| u16::from_le_bytes([ds[a], ds[a + 1]]);
        let g = game(0, 2);
        let w = World::from_game(&g, 1);
        for c in 0..13usize {
            assert_eq!(w.code_bits[c], w16(0x3176 + 2 * c), "code {c}");
        }
        assert_eq!(&w.code_bits[9..12], &[0, 0, 0], "bare parts have no AI kit bit");
        assert_eq!(w.code_bits[12], 0xFFFF, "the case is -1");
    }

    /// `CS:14a3..14b4`: with a free hand and **no** stick in the kit, a bare part under the feet
    /// is not touched; with the case in hand it is (it goes into the case).
    #[test]
    fn a_bare_part_is_taken_only_with_the_case_in_hand() {
        let mut c = Cpu::new(1, 0);
        c.intent[I_NAV] = 1;
        c.countdown = 5;
        let mut w = World::default();
        w.cell = [0, 0x0b];
        w.kit = [0, 0x800];
        let (_k, _s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
        assert_eq!(c.intent[I_GRAB], 0, "free hand, no stick: leave the part alone, {:?}", c.intent);
        let mut c = Cpu::new(1, 0);
        c.intent[I_NAV] = 1;
        c.countdown = 5;
        w.carried = [-1, 0x0c];
        let (_k, _s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
        assert_ne!(c.intent[I_GRAB], 0, "case in hand: take it, {:?}", c.intent);
    }

    /// The original only drives the black spy, so its absolute hand words (`0x2F2E` / `0x2F2C`)
    /// mean "mine" / "his". The port's white computer (`--cpu1`) must read them the same way:
    /// with the case in *its own* hand it walks to a part, it does not chase the other spy.
    #[test]
    fn the_hand_words_mean_mine_and_his_for_either_colour() {
        for me in 0..2usize {
            let mut w = World::default();
            w.carried = [-1, -1];
            w.carried[me] = 0x0c;
            w.screen = [4, 4];
            w.cell_col = [50, 50];
            w.cell_row = [2, 2];
            w.y = [46, 46];
            // a part lying in my own room, far to the west; the other spy stands to the east
            w.cell_col[1 - me] = 90;
            w.links[3] = Link { used: true, screen: 4, row: 2, col: 10 };
            let mut c = Cpu::new(me, 0);
            c.action = ACT_EXIT;
            let mut k = Keys::default();
            c.beh_nav(&w, &mut k, &mut Seq::of(&[0x00; 8]));
            assert!(k.left && !k.right, "colour {me}: case in hand -> walk west to the part, got {k:?}");
            // and the other way round: his hand holds the case and mine is empty -> chase him
            w.carried = [-1, -1];
            w.carried[1 - me] = 0x0c;
            let mut c = Cpu::new(me, 0);
            c.action = ACT_EXIT;
            let mut k = Keys::default();
            c.beh_nav(&w, &mut k, &mut Seq::of(&[0x00; 8]));
            assert!(k.right && !k.left, "colour {me}: he has the case -> chase east, got {k:?}");
        }
    }

    /// `DS:0x3176` is the *black* spy's item table (it is the only spy the original drives): the
    /// white plunger (1) and white bucket (4) are 0 there and the black ones (2, 5) carry the kit
    /// bits. For the port's white computer the pairs swap, or it would pick up the black spy's
    /// bucket, find it has no slot for it, drop it, and pick it up again for the rest of the round.
    #[test]
    fn each_colour_wants_its_own_plunger_and_bucket() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let g = game(0, 2);
        let b = World::from_game(&g, 1);
        let wh = World::from_game(&g, 0);
        assert_eq!((b.code_bits[1], b.code_bits[2], b.code_bits[4], b.code_bits[5]), (0, 1, 0, 4));
        assert_eq!((wh.code_bits[1], wh.code_bits[2], wh.code_bits[4], wh.code_bits[5]), (1, 0, 4, 0));
        for me in 0..2usize {
            let mut w = World::default();
            w.code_bits = if me == 1 { b.code_bits } else { wh.code_bits };
            let own = if me == 1 { 5u8 } else { 4 };
            let his = if me == 1 { 4u8 } else { 5 };
            w.cell[me] = own;
            let mut c = Cpu::new(me, 0);
            c.intent[I_NAV] = 1;
            c.countdown = 5;
            c.think(&w, &mut Seq::of(&[0x00; 8]));
            assert_ne!(c.intent[I_GRAB], 0, "colour {me}: his own bucket {own} is worth a press");
            w.cell[me] = his;
            let mut c = Cpu::new(me, 0);
            c.intent[I_NAV] = 1;
            c.countdown = 5;
            c.think(&w, &mut Seq::of(&[0x00; 8]));
            assert_eq!(c.intent[I_GRAB], 0, "colour {me}: the other colour's bucket {his} is not");
        }
    }

    /// Problem 3 of stage 17b, confirmed in stage 18d. `DS:0x2F8C` is `0x2F8A + si` for the black
    /// spy: the **"has owned the case"** word `CS:3890` sets when a spy takes the briefcase (the
    /// port's `case_owned`, stage 11) and *only* the round template restore `CS:125f` clears — so
    /// the case leaving the hand again does not clear it. `CS:1daa` reads it: cold, with two parts
    /// in the case and the case not in anybody's hand, an owner of the case walks to the exit (5)
    /// rather than the igloo (6).
    #[test]
    fn the_cold_branch_reads_whether_i_have_owned_the_case() {
        let mut w = World { heat: [3, 40], mask: 0x400 | 0x40 | 0x80, ..World::default() };
        w.heat[1] = 42;
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &w);
        assert_eq!(c.action, ACT_WARM, "never owned the case: warm up");
        w.case_owned = [false, true];
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &w);
        assert_eq!(c.action, ACT_EXIT, "owns the case and two parts are in it: head for the exit");
        // both hands empty is the ordinary case: the AI drops the case to stow a tool, and the
        // latch still says "this round's case is mine" (1,784 recorded dumps set, only 787 holding)
        w.carried = [-1, -1];
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &w);
        assert_eq!(c.action, ACT_EXIT, "the case dropped: still the latch, still the exit");
        w.carried = [-1, 0x0c];
        let mut c = Cpu::new(1, 0);
        decide(&mut c, &w);
        assert_eq!(c.action, ACT_WARM, "but with the case in his own hand he warms up first");
    }

    /// `DS:0x2F96` is read as `0x2F96 + si` (`CS:1c2b`): for the black spy that is **`0x2F98`, the
    /// trapulator slot the AI asked for**. The dynamite slot (0x0A) skips the final A press.
    #[test]
    fn the_item_behaviour_skips_the_last_press_for_the_dynamite_slot() {
        for (slot, fires) in [(4u16, true), (2, true), (0x0a, false)] {
            let mut c = Cpu::new(1, 0);
            c.intent[I_ITEM] = 0x0f; // the tail state
            c.target_slot = slot;
            c.countdown = 5;
            let w = World::default();
            let (k, _s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
            assert_eq!(k.fire, fires, "slot {slot:#x}");
        }
    }

    /// **The word is a pair, not a constant (stage 18d).** `CS:1c2b` is
    /// `cmpw $0xa,0x2f96(%si)`: the address is `si`-indexed like the rest of the AI's words, so
    /// the black spy (`si = 2`) reads **`0x2F98`** — the slot it asked for itself, written at
    /// `CS:13bf` (0x0a), `CS:13d1` (0x04), `CS:13e3` (0x02) and `CS:14fb` (0x0a) and zeroed by the
    /// round restore `CS:125f` — and the white spy (`si = 0`) reads **`0x2F96`**, which nothing in
    /// the whole image writes (0 in all 19,425 recorded dumps). The dump reader must index the
    /// pair; a hard-wired `0x2f98` is a constant where the original has a per-spy word.
    #[test]
    fn the_slot_word_is_each_spies_own_half_of_the_pair() {
        // the halves deliberately disagree, so a reader of the wrong one is visible
        let ds = dump(&[(0x2f96, 0x04), (0x2f98, 0x0a)]);
        assert_eq!(Cpu::from_dump(&ds, 1).target_slot, 0x0a, "black reads 0x2F96 + 2");
        assert_eq!(Cpu::from_dump(&ds, 0).target_slot, 0x04, "white reads 0x2F96 + 0");
        // and the tail's press follows whichever half was seeded
        let ds = dump(&[(0x2f96, 0x0a), (0x2f98, 0x0a)]);
        let w = World::default();
        let mut c = Cpu::from_dump(&ds, 1);
        c.intent[I_ITEM] = 0x0f;
        c.countdown = 5;
        let (k, _s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
        assert!(!k.fire, "slot 0x0a: no press (CS:1c30)");
    }

    /// The tail's A press reads the slot word out of the dump (`CS:1c2b` -> `CS:1c32`), so the
    /// **only** slot that skips it is `0x0A`. A constant 0 in its place would press on every
    /// frame and lose the `0x0A` case; a constant non-zero would never press.
    #[test]
    fn the_item_tails_press_follows_the_slot_word() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = exe();
        for (slot, fires) in [(0x0au16, false), (4, true), (2, true), (0, true)] {
            let ds = dump(&[(0x2f98, slot), (0x3243, 10), (0x2ed4, 0x0f)]);
            let mut c = Cpu::from_dump(&ds, 1);
            let w = World::from_dump(&ds, 1, &exe);
            let (k, _s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
            assert_eq!(k.fire, fires, "slot {slot:#x} (CS:1c2b cmpw $0xa)");
        }
    }

    /// **A free hand skips the stow, not the panel (stage 18d).** `CS:1b93` tests the hand first:
    /// `cmpw $0xffff,0x2f2e ; je 0x1bb0` — a spy whose hand is already empty goes to the *wait*
    /// arm, which opens the trapulator and walks the cursor to the slot (`CS:1bde`). The port fell
    /// through to the `CS:1c2b` tail instead, so an item errand started with a free hand — which is
    /// every fidget's tool (`CS:1366` takes tools out of the kit, not out of the hand) — ended on a
    /// single stray A press with the panel never opened.
    #[test]
    fn an_item_start_with_a_free_hand_opens_the_panel() {
        let mut c = Cpu::new(1, 0);
        c.intent[I_ITEM] = 1;
        c.target_slot = 4;
        c.countdown = 5;
        let w = World::default(); // an empty hand, nothing under the feet
        let (k, s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
        assert_eq!(s.trap_open, Some(true), "the panel is opened (CS:1bde)");
        // stage 20c: the panel opens *on* the slot, so there is no RIGHT walk and no A yet
        assert_eq!(s.trap_cursor, Some(4), "the panel opens on the requested slot");
        assert!(!k.right && !k.fire, "no key on the opening frame: {k:?}");
        assert_eq!(c.intent[I_ITEM], 0x0b, "in the select, got {:#x}", c.intent[I_ITEM]);
        assert_eq!(c.cooldown, 0, "the behaviour has not ended");
        // the case in hand is a full hand: it is stowed first, exactly as before
        let mut c = Cpu::new(1, 0);
        c.intent[I_ITEM] = 1;
        c.target_slot = 4;
        c.countdown = 5;
        let w = World { carried: [-1, 0x0c], ..World::default() };
        let (_k, s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
        assert!(s.stow, "the case is put away first");
        assert_eq!(c.intent[I_ITEM], 2);
    }

/// **stage 20c — the slot request places the cursor; there is no visible walk.**
///
/// The recorded panel frames have the cursor on the requested slot the moment the panel is up
/// (`0x2F3E == 0x2F98`: 10/10 in `ai-d2-18e` ds_00080 and `audio-smoke` ds_00052, 2/2 in
/// `m3-heat-90s` ds_00122) and the panel is up for one dump (0.5–1.1 % of a d1/d2 round against
/// 1–5 errands). A walk from the slot the last take left the cursor on would need three frames
/// per slot (`DS:0x2F9E`, `CS:2af6`), so the errand has to place the word itself: `0x2F3C` is
/// written outside the panel by `FUN_26c0_315f` (`CS:31aa`, table `0x2D7A`) and the
/// "the slot is already chosen" arm is `DS:0x2F9C` (`CS:1bb0` -> `CS:1bde`).
#[test]
fn the_slot_request_places_the_cursor_before_the_panel_appears() {
    let mut c = Cpu::new(1, 0);
    c.intent[I_ITEM] = 0x0b; // the select, which is where the errand lands with a free hand
    c.target_slot = 4;
    c.countdown = 5;
    // the panel shut, the cursor left on the map's slot by a previous errand
    let w = World { trap_open: [false, false], trap_cursor: [0, 0x0c], ..World::default() };
    let (k, s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
    assert_eq!(s.trap_open, Some(true), "the panel is opened (CS:1be4)");
    assert_eq!(s.trap_cursor, Some(4), "and it opens on the slot that was asked for");
    assert!(!k.right && !k.fire, "nothing is pressed on the frame it opens: {k:?}");
    // next frame the world already holds the slot: the A press goes out
    let w2 = World { trap_open: [false, true], trap_cursor: [0, 4], ..World::default() };
    let (k2, s2) = c.think(&w2, &mut Seq::of(&[0x00; 8]));
    assert!(k2.fire, "A on the slot (CS:1bfa)");
    assert_eq!(s2.trap_cursor, None, "nothing left to place");
}

/// The map errand opens on slot 12 the same way (`CS:1b19` compares the cursor against `0x0C`).
#[test]
fn the_map_errand_opens_the_panel_on_the_map_slot() {
    let mut c = Cpu::new(1, 0);
    c.intent[I_MAP] = 5; // 0x2ED0 = 5, the map's panel state (`CS:1b03`)
    c.countdown = 5;
    let w = World { trap_open: [false, false], trap_cursor: [0, 0], ..World::default() };
    let (k, s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
    assert_eq!(s.trap_open, Some(true));
    assert_eq!(s.trap_cursor, Some(0x0c), "the map's own slot");
    assert!(!k.right, "no walk from slot 0 to 12");
}

    /// **The case latch has two readers (stage 18d).** `DS:0x2F8C` (`0x2F8A + si`) is set by the
    /// briefcase pickup (`CS:3890`, only for a picked-up code `>= 0x0c`) and cleared **only** by
    /// the per-round template restore (`CS:125f`) — never when the case is dropped or stowed. Its
    /// second reader is `CS:14d5`, between the dynamite gate and the draw: a part under the feet is
    /// no longer worth a dynamite trap once the case has been mine this round.
    #[test]
    fn the_case_latch_gates_the_dynamite_arm_after_the_case_leaves_the_hand() {
        // a part under the feet, both hands empty, the dynamite in the kit
        let base = World {
            mask: 0x400,
            cell: [0, 11],
            kit: [0, 0x20],
            carried: [-1, -1],
            ..World::default()
        };
        let start = |owned: bool| {
            let mut c = Cpu::new(1, 0);
            c.cooldown = 5; // keeps the idle fidget out of the arms under test
            c.intent[I_NAV] = 1;
            c.countdown = 5;
            let w = World { case_owned: [false, owned], ..base.clone() };
            let (_k, _s) = c.think(&w, &mut Seq::of(&[0xff; 8]));
            c
        };
        let never = start(false);
        assert_eq!(never.target_slot, 0x0a, "never owned the case: set the dynamite up (CS:14df)");
        assert_ne!(never.intent[I_ITEM], 0, "the errand is queued, not abandoned");
        let owned = start(true);
        assert_eq!(owned.intent[I_ITEM], 0, "the latch is set: leave it, no trap (CS:14d5)");
        assert_eq!(owned.target_slot, 0, "and no slot was asked for");
        // the case in my own hand is a *different* word (`0x2F2E`) and a different arm
        let mut c = Cpu::new(1, 0);
        c.cooldown = 5;
        c.intent[I_NAV] = 1;
        c.countdown = 5;
        let w = World { carried: [-1, 0x0c], ..base.clone() };
        let (_k, _s) = c.think(&w, &mut Seq::of(&[0xff; 8]));
        assert_ne!(c.intent[I_GRAB], 0, "the case in hand takes the part (CS:14aa)");
    }

    /// **`DS:0x34A0` / `DS:0x3496` are the speaker, not the AI (stage 18d).** They are the state
    /// word and pending-cue pointer of the PC-speaker sequencer in the INT 1Ch handler at `CS:4196`
    /// (installed by `CS:4548`, its `DS` patched by the MZ relocation at `CS:419f`, so it is the
    /// game's own segment). Game code reads `0x34A0` only as the 19x pre-empt gate whose guarded
    /// body is the two cue stores; the AI reads neither word, so a round whose cue words differ
    /// must make the same decision and emit the same keys.
    #[test]
    fn the_speaker_state_is_not_an_ai_input() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let quiet = dump(&[(0x2ece, 1), (0x2ed0, 1), (0x3243, 10), (0x2f98, 0x0a)]);
        let noisy = dump(&[
            (0x2ece, 1),
            (0x2ed0, 1),
            (0x3243, 10),
            (0x2f98, 0x0a),
            (0x34a0, 3),
            (0x3496, 0xa5e6),
            (0x3494, 0xa15a),
            (0x3498, 0x3716),
            (0x349c, 0xc8),
            (0x349e, 0x0a),
        ]);
        let exe = exe();
        for player in 0..2usize {
            for live in [true, false] {
                let mut a = Cpu::from_dump(&quiet, player);
                let mut b = Cpu::from_dump(&noisy, player);
                if !live {
                    a.clear_intents();
                    b.clear_intents();
                }
                let (ka, sa) = a.think(&World::from_dump(&quiet, player, &exe), &mut Seq::of(&[0x40; 8]));
                let (kb, sb) = b.think(&World::from_dump(&noisy, player, &exe), &mut Seq::of(&[0x40; 8]));
                assert_eq!(ka, kb, "player {player} live {live}: the cue words must not reach the keys");
                assert_eq!(sa, sb, "player {player} live {live}: nor the AI's direct writes");
                assert_eq!(a.action, b.action, "player {player} live {live}: nor the decision");
                assert_eq!(a.intent, b.intent, "player {player} live {live}: nor the intents");
            }
        }
    }

    /// `CS:1b93..1ba4` / `CS:1ac6..1ad7`: both the item and the map behaviour call the stow
    /// (`CS:315f`) **and look at the hand afterwards**: still full (the stow dropped nothing - no
    /// drop is allowed above `y = 0x24` or indoors) means the whole behaviour is abandoned
    /// (`CS:1c3a`/`CS:1b4d`: intent 0, cooldown 0x14). The port used to carry on regardless, open
    /// the panel and take a tool *over* the case in hand, which loses the case for the round.
    #[test]
    fn a_stow_that_leaves_the_hand_full_abandons_the_behaviour() {
        for which in [I_ITEM, I_MAP] {
            let mut c = Cpu::new(1, 0);
            c.intent[which] = 1;
            c.countdown = 5;
            c.target_slot = 4;
            let mut w = World { carried: [-1, 0x0c], ..World::default() };
            w.stow_ok = [true, false];
            let (_k, s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
            assert!(s.stow, "the stow is still attempted");
            assert_eq!(c.intent[which], 0, "behaviour {which}: abandoned, got {:?}", c.intent);
            assert_eq!(c.cooldown, 0x14);
            // and when the stow works the behaviour carries on to state 2
            let mut c = Cpu::new(1, 0);
            c.intent[which] = 1;
            c.countdown = 5;
            w.stow_ok = [true, true];
            let (_k, _s) = c.think(&w, &mut Seq::of(&[0x00; 8]));
            assert_eq!(c.intent[which], 2, "behaviour {which}");
        }
    }

    /// **The wall stall (stage 17b).** The original's AI never sees terrain: `CS:1774` reads the
    /// object byte ahead, `DS:0x2FA4` (the *other spy's body*, not a wall) and the code under the
    /// feet. It gets round the rocks with the codes `0x16`/`0x17` that flank them
    /// (`CS:164c` presses UP on them) and with the object-byte side-step. Feeding the engine's
    /// own terrain veto into the wall branch (stage 17's "deviation 4") made it side-step in place
    /// for ever: on island 5 at difficulty 1, seed 17, the computer oscillated at one spot from
    /// frame ~2,970 until the cold froze it. This runs 4,500 frames and fails on any stand-still
    /// of 200 frames while the other spy is away.
    #[test]
    fn the_computer_gets_round_the_rocks_without_terrain_help() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let mut g = Game::new(
            exe(),
            &RoundOpts { island: 4, players: 2, difficulty: 0, seed: 17, cpu: [false, true], ..RoundOpts::default() },
        )
        .expect("round");
        // a dither = 400 consecutive frames inside a 12 x 16 box, outdoors, not animating, with
        // the other spy away, while the computer is not warming
        let mut win: Vec<(u16, i32, i32)> = Vec::new();
        let mut worst = 0usize;
        let mut at = (0u16, 0, 0);
        for _ in 0..4500u32 {
            for _ in 0..26 {
                g.tick(crate::game::Input::default());
            }
            if g.phase != crate::game::Phase::Playing || g.spies[1].launched {
                break;
            }
            let s = &g.spies[1];
            let busy = s.anim.is_some() || s.pose_raised || s.frozen != 0 || s.screen >= 10;
            let near = g.spies[0].screen == s.screen && (g.spies[0].x() - s.x()).abs() <= 24;
            if busy || near {
                win.clear();
                continue;
            }
            win.push((s.screen, s.x(), s.y()));
            if win.len() > 400 {
                win.remove(0);
            }
            if win.len() == 400 {
                let (x0, x1) = (win.iter().map(|p| p.1).min().unwrap(), win.iter().map(|p| p.1).max().unwrap());
                let (y0, y1) = (win.iter().map(|p| p.2).min().unwrap(), win.iter().map(|p| p.2).max().unwrap());
                if win.iter().all(|p| p.0 == win[0].0) && x1 - x0 <= 12 && y1 - y0 <= 16 {
                    worst += 1;
                    at = (win[0].0, x0, y0);
                }
            }
        }
        assert_eq!(worst, 0, "dithered in a 12 x 16 box for 400 frames, from {:?}", at);
    }

    /// `CS:1794..17bd` -> `CS:1791`: standing on the ice plates' edge codes (0x31/0x33 with the
    /// left key, 0x32/0x34 with the right) the computer backs off with the **scenery** response
    /// (`CS:1862`: left and right cleared, DOWN), not with the wall branch's y-based side-step.
    #[test]
    fn the_ice_edge_codes_use_the_scenery_response() {
        for (ground, left) in [(0x31i16, true), (0x33, true), (0x32, false), (0x34, false)] {
            let mut c = Cpu::new(1, 0);
            let w = World {
                ground: [0, ground],
                y: [46, 46], // y > 0x2B: the wall branch would say UP
                cell_col: [50, if left { 60 } else { 40 }],
                ..World::default()
            };
            // a draw below the threshold -> the response fires
            let k = c.navigate_r(&w, w.screen[1], 50, 2, &mut Seq::of(&[0x00; 8]));
            assert!(k.down && !k.up && !k.left && !k.right, "ground {ground:#x}: scenery -> DOWN only, got {k:?}");
        }
    }

    // ---- the engine wiring -----------------------------------------------------------------

    #[test]
    fn a_one_player_round_gives_the_second_spy_the_computer() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        // `--players 0` / the setup screen's one-player option: both spies in the round, the
        // second one driven by `cpu[1]`.
        let g = Game::new(
            exe(),
            &RoundOpts { island: 0, players: 2, cpu: [false, true], ..RoundOpts::default() },
        )
        .expect("round");
        assert!(g.cpu[0].is_none(), "spy 1 is the human");
        let c = g.cpu[1].as_ref().expect("spy 2 is the computer in a one-player round");
        assert_eq!(c.player, 1);
        let g = game(0, 2);
        assert!(g.cpu[1].is_none(), "two players: no computer at all");
        // and the engine actually substitutes its keys for that spy
        let mut g = Game::new(
            exe(),
            &RoundOpts { island: 0, players: 2, cpu: [false, true], ..RoundOpts::default() },
        )
        .expect("round");
        let before = (g.spies[1].x(), g.spies[1].y());
        for _ in 0..26 * 40 {
            g.tick(crate::game::Input::default());
        }
        assert_ne!((g.spies[1].x(), g.spies[1].y()), before, "the computer moved its spy");
    }
}
