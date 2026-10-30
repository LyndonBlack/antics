//! Items: the object byte, the carried word, and the Trapulator's tables.
//!
//! Stage 7. Everything here is either **read** out of `spy.exe` (a table address, a mask, a routine's
//! branch) or **measured** off the recorded data-segment dumps. The evidence for each constant is in
//! `recon/round/port_check/STAGE7_REPORT.md`; the code references are Ghidra names from
//! `recon/disasm/spy_decompiled.c`, disassembled with `recon/round/disasm_cs.py`.
//!
//! The model, in one paragraph. An object byte holds an *item code* in its low five bits; bit 7 means
//! "under a mound"; bits 5 and 6 tag a stick of dynamite as white's or black's (the two words
//! `DS:0x3307` = 0x20 and `DS:0x3309` = 0x40). A spy carries at most one thing, its code in
//! `DS:0x2F2C` (`0xFFFF` = empty); the seven trapulator tools live as bits of the kit word
//! `DS:0x32A3`, and the three rocket parts, once in the briefcase, as bits of `DS:0x2EB8`. Fire
//! standing on a byte takes it (`FUN_26c0_373b`); fire with the briefcase in hand over a part puts
//! the part *into* the case (`0x3822`'s `code >= 9` branch); a direction press while fire is held
//! buries it (`| 0x80`, `FUN_26c0_38e1` with `BX = 1`); a second tap of fire with nothing to pick up
//! opens the trapulator, and fire there takes the selected slot (`FUN_26c0_2a4e`).
//!
//! Digging is not a separate action: a mound byte `0x89` has item code 9, so the ordinary pickup
//! takes part 9 and clears the byte. 171 recorded digs do exactly that.


/// **read** — `DS:0x260A`, four words: row index -> byte offset inside a block (`0, 0x9C, 0x138,
/// 0x1D4`). `FUN_26c0_2549` computes the object cell with it.
pub const OBJ_ROW_OFFS: usize = 0x260A;

/// The carried word's "nothing" value (`DS:0x2F2C` holds 0xFFFF when the hand is empty).
use crate::round;

pub const CARRY_NONE: u16 = 0xFFFF;
/// The low five bits of an object byte: the item code (`& 0x1F` in every branch that reads one).
pub const CODE_MASK: u8 = 0x1F;
/// Bit 7 of an object byte: hidden under a mound. Set by `FUN_26c0_257b` for the island's buried
/// items and by the bury branch of `FUN_26c0_38e1` (`OR 0x80`).
pub const BURIED: u8 = 0x80;

/// **read** — `DS:0x2D60`, 13 words: item code -> the bit it owns in the kit word and in `0x2EB8`.
/// `[0, 1, 1, 2, 4, 4, 8, 0x10, 0x20, 0x40, 0x80, 0x200, 0x400]`.
pub const DS_CODE_BIT: usize = 0x2D60;
/// **read** — `DS:0x2D8C`, 7 words, trapulator slot -> kit bit: `1, 2, 4, 8, 0x10, 0x20, 0x800`.
pub const DS_SLOT_BIT: usize = 0x2D8C;
/// **read** — `DS:0x2D9A`, 13 words, indexed by `cursor + player`: the item code a slot holds.
/// Slot 0 is the plunger (1 white / 2 black), slot 2 the saw (3/3), slot 4 the water bucket
/// (4/5), so the two players' own tools differ: `[1,2, 3,3, 4,5, 6,6, 7,7, 8,8]`, then `1` for the
/// map slot.
pub const DS_SLOT_CODE: usize = 0x2D9A;
/// **read** — `DS:0x2DBA` (player 1) and `DS:0x2DD4` (player 2), 13 words, item code -> *this
/// player's* version of it. `FUN_26c0_315f` stows only when `table[code] == code`: the other spy's
/// plunger or bucket is dropped instead, because the trapulator has no slot for it.
pub const DS_CODE_CANON: usize = 0x2DBA;
pub const CODE_CANON_STRIDE: usize = 0x1A;
/// **read** — `DS:0x2D7A`, 13 words, item code -> the trapulator cursor (`slot * 2`) it lives in.
pub const DS_CODE_CURSOR: usize = 0x2D7A;
/// **read** — `DS:0x2D08`, 13 words, item code -> the pose *set* the spy is drawn with while holding
/// it (`0x24` for an empty hand). `FUN_26c0_315f`/`2a4e`/`373b` all write `DS:0x2EF8` from it.
pub const DS_CODE_POSE: usize = 0x2D08;
/// **read** — `DS:0x3307` (player 1) / `DS:0x3309` (player 2): the owner tag of a dynamite stick,
/// 0x20 for white and 0x40 for black. Read in the pickup's ownership tests and `OR`ed into a buried
/// stick by the bury branch.
pub const DS_PLAYER_TAG: usize = 0x3307;
/// **read** — `DS:0x32D3`, two words: `[2, 0]`. The detonator branch does
/// `mov 0x32d3(%si),%si`, i.e. it turns the firer's `SI` into the **other** spy's, and then reads
/// that spy's cached object cell (`0x32D7+si`) — the cell the plunger is aimed at.
pub const DS_OTHER_PLAYER: usize = 0x32D3;

/// The two owner tags as the bits they are in an object byte: `0x60` is `0x3307 | 0x3309`, and
/// `CS:3930` tests exactly that pair when it decides whether a stick may be set on a cell.
pub const OWNER_WHITE: u8 = 0x20;
pub const OWNER_BLACK: u8 = 0x40;
pub const OWNER_TAGS: u8 = OWNER_WHITE | OWNER_BLACK;

/// The kit word's "briefcase in hand" bit (`0x2D60[12]`). Measured: it equals `[0x2F2C] == 12` on
/// 2477/2477 dumps of `trapulator-island4`.
pub const KIT_CASE_BIT: u16 = 0x400;
/// The kit word's tool bits plus the map: `kit &= 0x83F` is the mask `FUN_26c0_38e1` applies when
/// the briefcase is put down (clearing the part mirrors and the case bit).
pub const KIT_TOOLS: u16 = 0x83F;
/// The map's slot. `FUN_26c0_2a4e` treats cursor 12 as the map rather than an item.
pub const SLOT_MAP: u16 = 12;
/// Dynamite's item code, and the ceiling `FUN_26c0_315f` keeps on the stick count (`DS:0x330F`).
pub const CODE_DYNAMITE: u16 = 8;
pub const DYN_MAX: u16 = 15;
/// The two tools whose code is the **same for both spies**: the saw (`0x2D9A`'s slots 2 and 3 are
/// both 3) and the ice pick (slots 6 and 7 are both 6). The plunger and the bucket are per player
/// (`0x2DB2+si` = 1/2 and `0x2DB6+si` = 4/5) and are read out of the table, not hard-coded.
pub const CODE_SAW: u16 = 3;
pub const CODE_ICE_PICK: u16 = 6;


// ---- the hazards the three tools leave in the object block (stage 13a) ----------------------

/// The water bucket's spill (`CS:28fa`): a slip for whoever steps on it (`CS:2473`).
pub const OBJ_SPILL: u8 = 0x0D;
/// The saw's hole (`CS:28e9`): the spy who steps in falls through (`CS:24ac`).
pub const OBJ_HOLE: u8 = 0x0E;
/// The ice pick's icicle (`CS:2f55`): three bytes on the door's row, knocked out on contact
/// (`CS:24eb`). Codes 0x14..0x7F are skipped by the object layer's *signed* `CMP AL,0x14; JGE`
/// (`CS:0c17`), so the icicle is invisible: the original draws it separately, from `FUN_26c0_207b`.
pub const OBJ_ICICLE: u8 = 0x14;
/// The last item code: `CMP AL,0xC; JG return` in the pickup refuses everything above this, so
/// 0x0D/0x0E/0x14 are the first three bytes a spy can neither take nor bury.
pub const CODE_LAST_ITEM: u16 = 12;


// ---- the kit box and the trapulator's cursor (stage 8) ---------------------------------------

/// **read** — `DS:0x322D`, 4 words: item code 9 + n -> the **owner** of that part. `0xFFFF` = not
/// found by anyone, `0` = the white spy, `2` = the black spy (`SI`, which is 0 or 2). Written by
/// `FUN_26c0_373b` at `CS:383a` (deposit) and `CS:3886` (pickup) with `mov %si,ds:0x322d(%bp)`,
/// `bp = 2*(code-8)`. Unlike the deposited mask (`0x2EB8`, shared by both spies) this ledger is
/// **per spy**, which is what makes the kit box correct for the black spy.
pub const DS_PART_OWNER: usize = 0x322D;
/// **read** — `DS:0x3219`, 4 words: the part's position record, cleared to `0xFFFF` whenever the
/// part is taken or deposited (`CS:383f`, `CS:387f`) and filled in by `FUN_26c0_39b0` when a part
/// is dropped (screen, row, column, cell pointer at `0x3205`/`0x320F`/`0x3223`). **Not drawn by
/// anything the port models yet** — the map screen is what reads it — so the port records the
/// address and nothing more.
#[allow(dead_code)]
pub const DS_PART_POS: usize = 0x3219;
/// Nobody owns the part (`DS:0x322D`'s initial value).
pub const OWNER_NONE: u16 = 0xFFFF;

/// **read** — the kit box's three cells, in drawing order (`FUN_26c0_31c8`'s `bp` = 0x16, 0x14,
/// 0x12, stepping down to 0x12): **left = code 11, middle = code 10, right = code 9**.
pub const KIT_CELL_CODES: [u16; 3] = [11, 10, 9];
// The kit box's canvas (`DS:0x1494`'s own header `0A 0B`, blitted at row `DS:0x249E[player]` =
// 76 / 184, byte column `0x0A` = x 40..79) is drawn by `crate::chrome::draw_kit_box` from the
// records themselves; the five `KIT_BOX_*` constants that described it for the captured-image
// blit went with that blit in stage 29 (`chrome::KIT_X` / `KIT_ROW` / `KIT_W` / `KIT_BYTES` are
// the same geometry, in the byte columns the code uses).

/// **read** — `DS:0x2F4C` (per player): the kit box's blink phase. `FUN_26c0_31c8` negates it once
/// per drawn frame (`negw 0x2f4c(%si)` at `CS:325a`) and only stamps the *found* icons when it is
/// non-negative, so a part that is in the briefcase flashes at 10 Hz while the box is drawn.
pub const DS_KIT_BLINK: usize = 0x2F4C;
/// **read** — `DS:0x2F9E` (per player): the trapulator cursor's repeat delay. `FUN_26c0_2a4e`
/// moves the cursor when this is zero and then reloads it with 2, so a held key steps the cursor
/// every third frame (300 ms) and a fresh press moves at once.
pub const DS_CURSOR_DELAY: usize = 0x2F9E;

/// **read** — `DS:0x15BC` (blitted when the open word is **negative**) and `DS:0x15CA` (when it is
/// non-negative): 13 pointers each — `word[table + cursor]` — to a 4-row patch of the slot's own dot
/// cell. `0x15BC` holds the patch with the 2x2 core **lit**, `0x15CA` the one with it **dark**, so
/// the selected slot's light blinks at 10 Hz.
pub const DS_HL_PHASE_POS: usize = 0x15BC;
pub const DS_HL_PHASE_NEG: usize = 0x15CA;
/// **read** — `DS:0x2E0A` (`+2*player`): the mark's row, 0x4A = 74 (top console) or 0xB6 = 182.
pub const DS_HL_ROW: usize = 0x2E0A;
/// **read** — `DS:0x2E0E` (`+cursor`, a word table of 7 entries): the mark's **byte column** (2 bpp,
/// so x = 4*value): 25, 31, 36, 42, 47, 53, 58 -> x = 100, 124, 144, 168, 188, 212, 232. Measured
/// agreement: the lit dot cores of `profile/rois.toml` sit at x 103, 126, 148, 169, 191, 213, 235,
/// i.e. 3 px into each 16-px patch — the patch starts at the cell's left edge, not at its core.
pub const DS_HL_COL: usize = 0x2E0E;

/// The owner slot of a rocket part (`DS:0x322D`'s index): `code - 8`. The original indexes the
/// ledger with `bp = 2*(code-8)` for codes 9..12, so it is five words wide (0x322D..0x3236) even
/// though only 9, 10 and 11 are ever read back.
pub fn part_owner_slot(code: u16) -> Option<usize> {
    if (8..=12).contains(&code) {
        Some((code - 8) as usize)
    } else {
        None
    }
}

/// How many words the owner ledger spans (`DS:0x322D..0x3236`).
pub const PART_OWNERS: usize = 5;

/// The object byte's row base: `(y - 28) >> 3`, indices 0..3 into `DS:0x260A`.
pub const OBJ_ROW0_Y: i32 = 28;

fn word(ds: &[u8], addr: usize) -> u16 {
    let a = addr & 0xFFFF;
    u16::from_le_bytes([ds[a], ds[(a + 1) & 0xFFFF]])
}

/// The bit of the kit word / `0x2EB8` an item code owns, through the executable's own table.
pub fn code_bit(ds_img: &[u8], code: u16) -> u16 {
    if code > 12 {
        return 0;
    }
    word(ds_img, DS_CODE_BIT + 2 * code as usize)
}

/// The trapulator slot's own kit bit (`0x2D8C`), for cursor `slot*2`.
pub fn slot_bit(ds_img: &[u8], cursor: u16) -> u16 {
    if cursor > SLOT_MAP || cursor % 2 != 0 {
        return 0;
    }
    word(ds_img, DS_SLOT_BIT + cursor as usize)
}

/// The item code a trapulator slot holds for `player` (0 = white, 1 = black).
///
/// `FUN_26c0_2a4e` at `CS:2aa3` does
///
/// ```text
///   2aa3  mov  %bx,%bp        ; bp = cursor (0, 2, .. 12)
///   2aa5  shl  $1,%bp         ; bp = 2*cursor
///   2aa7  add  %si,%bp        ; bp += SI  -- the player's own base, 0 (white) or 2 (black)
///   2aa9  mov  %ds:0x2d9a(%bp),%bp
/// ```
///
/// so the byte address is `0x2D9A + 2*cursor + 2*player`, i.e. **word index `cursor + player`**.
/// Stage 7 read it as `0x2D9A + 2*cursor + player` (byte offset `BP = cursor*2 + player`), which is
/// the same thing only for the white spy: for black it reads a word straddling two entries and hands
/// back a value like `0x0500` instead of a code. That is the black-spy trapulator fault of stage 8.
/// The table itself (`[1,2, 3,3, 4,5, 6,6, 7,7, 8,8]`) confirms the intent from the other side:
/// white's plunger is slot 0 of the even entries and black's is slot 0 of the odd ones.
pub fn slot_code(ds_img: &[u8], cursor: u16, player: usize) -> u16 {
    word(ds_img, DS_SLOT_CODE + 2 * cursor as usize + 2 * player)
}

/// `FUN_26c0_315f`'s ownership test: the player's own version of an item code.
pub fn canon_code(ds_img: &[u8], player: usize, code: u16) -> u16 {
    if code > 12 {
        return code;
    }
    word(ds_img, DS_CODE_CANON + player * CODE_CANON_STRIDE + 2 * code as usize)
}

/// The pose set the spy is drawn with while holding `code` (`0x24` for an empty hand).
pub fn code_pose(ds_img: &[u8], code: u16) -> u16 {
    if code == CARRY_NONE || code > 12 {
        return 0x24;
    }
    word(ds_img, DS_CODE_POSE + 2 * code as usize)
}

/// The trapulator cursor an item lives in.
pub fn code_cursor(ds_img: &[u8], code: u16) -> u16 {
    if code > 12 {
        return 0;
    }
    word(ds_img, DS_CODE_CURSOR + 2 * code as usize)
}

/// The dynamite owner tag of a player: `DS:0x3307` = 0x20 (white), `0x3309` = 0x40 (black).
pub fn player_tag(ds_img: &[u8], player: usize) -> u8 {
    word(ds_img, DS_PLAYER_TAG + 2 * player) as u8
}

/// The other player's index, through the executable's own table (`DS:0x32D3` = `[2, 0]`, so the
/// word holds the other spy's `SI`, i.e. `2 * player`).
pub fn other_player(ds_img: &[u8], player: usize) -> usize {
    (word(ds_img, DS_OTHER_PLAYER + 2 * player) / 2) as usize
}

/// `FUN_26c0_2549` — the object cell under a spy, as an index into the data segment.
///
/// ```text
///   block = word[0x2600 + screen]      ; screen is even; 0..8 are the five outdoor bands
///   row   = word[0x260A + 2*((y-28)>>3)]
///   cell  = block + row + (x >> 1)
/// ```
///
/// Verified against the recorded dumps: at screen 4, x = 71, y = 42 the phone gives
/// `0x589C + 0x9C + 35 = 0x595B`, which is what `DS:0x32D7` holds in that dump.
pub fn cell_addr(ds: &[u8], screen: u16, x: i32, y: i32) -> Option<usize> {
    if screen > 8 {
        return None;
    }
    let block = word(ds, round::DS_BLOCK_PTR + screen as usize) as usize;
    let row_i = (y - OBJ_ROW0_Y) >> 3;
    if !(0..4).contains(&row_i) {
        return None;
    }
    let row = word(ds, OBJ_ROW_OFFS + 2 * row_i as usize) as usize;
    let entry = (x >> 1).max(0) as usize;
    if entry >= 0x9C {
        return None;
    }
    let addr = block + row + entry;
    if addr + 1 < ds.len() {
        Some(addr)
    } else {
        None
    }
}

/// The same cell one entry further on, in the direction the spy faces: `FUN_26c0_373b` and the bury
/// branch both step `DI` by ±1, which is 2 world units, one object entry.
pub fn cell_ahead(ds: &[u8], screen: u16, x: i32, y: i32, mirror: bool) -> Option<usize> {
    let feet = cell_addr(ds, screen, x, y)?;
    let a = if mirror { feet.checked_sub(1)? } else { feet + 1 };
    if a < ds.len() {
        Some(a)
    } else {
        None
    }
}

/// **read** — a sprite in the executable's own format: **byte 0 = rows** (scanlines), **byte 1 =
/// width in bytes** (4 pixels each), then `rows*width` bytes of 2 bpp pixels, leftmost pixel in the
/// high bits.
///
/// The header order is the blitter's, not a guess: `FUN_26c0_45b0` takes the first byte into `BX`
/// and dispatches through the jump table at `DS:0x37E6`, whose entries step by 12 bytes — one 12-byte
/// unit of the unrolled copy chain, i.e. **one scanline pair** of `width` bytes, `add %bp` between
/// the pair (`0x2000 - width`, the odd CGA plane) and `sub %dx` after it (`0x1FB0 + width`, the next
/// row pair). So the first byte counts scanline pairs and the second is the row's byte width.
///
/// Stage 8 read the pair as (width, rows). That transposes every mark the trapulator draws: a
/// 4-row x 4-px mark became a 1-row x 16-px mark, which is the owner's "second light up and to the
/// right" (marks 0/2/4/6 have `w=2` and became 16 px wide with the cyan pixels at +5/+6 and +9/+10)
/// and, for the 1-byte-wide marks (slots 3 and 5), his "splits the light into 3 horizontal lines"
/// (the extra row plus the two dot rows underneath). See `STAGE8B_REPORT.md` §2.
pub fn sprite_at(ds: &[u8], addr: usize) -> Option<(usize, usize, &[u8])> {
    let a = addr & 0xFFFF;
    let (rows, w) = (*ds.get(a)? as usize, *ds.get(a + 1)? as usize);
    let n = w.checked_mul(rows)?;
    let data = ds.get(a + 2..a + 2 + n)?;
    Some((w, rows, data))
}

/// The mark `FUN_26c0_2cda` draws over the trapulator's selected slot, as
/// (x, y, byte width, rows, pixels).
///
/// ```text
/// 2ce8/2d04  bp   = DS:0x2F3C (+SI)              ; the cursor, 0..12
/// 2cec/2d08  bx   = DS:0x2E0A (+SI)              ; the row: 74 (white) / 182 (black)
/// 2cf0/2d0c  cx   = word[DS:0x2E0E + bp]         ; the **byte** column -> x = 4*cx
/// 2cf5       si   = word[DS:0x15CA + bp]         ; 0x2F38 >= 0
/// 2d11       si   = word[DS:0x15BC + bp]         ; 0x2F38 <  0
/// 2cfc/2d18  call 0x45b0 (blit at di = row_offset[bx] + cx)
/// 2d1c       negw 0x2F38(+SI)                    ; flip the phase for the next drawn frame
/// ```
///
/// So the mark is a patch of the slot's **own dot cell** — 4 rows tall, 2 bytes (8 px) wide for the
/// six tool slots and 1 byte (4 px) wide for the pickaxe and the dynamite — and the two tables are
/// the same cell with the dot's 2x2 core **lit** (`0x15BC`, the negative phase) and **dark**
/// (`0x15CA`, the non-negative one). Drawing it *over* the dot strip is what makes the selected
/// slot's light blink at 10 Hz; the mark is not a separate glyph above the strip.
///
/// Verified pixel for pixel against the recorded panel-open frames (91 in `playtest5`): the recorded
/// cell is `22200222 / 22011022 / 22011022 / 22200222` (slot 0, lit) and `... / 22000022 / ...`
/// (dark), which is exactly `A8 2A A1 4A A1 4A A8 2A` and `A8 2A A0 0A A0 0A A8 2A` at 0x1584 and
/// 0x15A8. See `recon/round/port_check/stage8b_mark.py`.
pub fn cursor_mark(ds: &[u8], cursor: u16, player: usize, negative: bool) -> Option<Mark> {
    if cursor > SLOT_MAP || cursor % 2 != 0 {
        return None;
    }
    // `jl 0x2d03` at CS:2ce5 sends the **negative** phase to 0x15BC and the non-negative one to
    // 0x15CA. The recorded panel-open frames say the opposite: of `playtest5`'s 87 settled
    // panel-open dumps, 67 record the *lit* patch next to a **non-negative** 0x2F3A and 20 record it
    // next to a negative one (`stage8b_mark.py`). Flipped the other way the split is 20/87. The two
    // tables are the lit and the dark patch of the same cell, so the difference is one half of the
    // 10 Hz blink — invisible in play — but the measurement is the game's own output, so it wins
    // here and the branch reading is recorded as an open item in STAGE8B_REPORT.md §2.
    let table = if negative { DS_HL_PHASE_NEG } else { DS_HL_PHASE_POS };
    let addr = word(ds, table + cursor as usize) as usize;
    let (w, h, data) = sprite_at(ds, addr)?;
    let x = 4 * word(ds, DS_HL_COL + cursor as usize) as usize;
    let y = word(ds, DS_HL_ROW + 2 * player) as usize;
    Some(Mark { x, y, w, h, data: data.to_vec() })
}

/// A sprite lifted out of the executable, positioned on the screen.
pub struct Mark {
    pub x: usize,
    pub y: usize,
    /// Width in **bytes** (4 pixels each).
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Mark {
    /// This sprite's pixel at (dx, dy) as a palette index, or `None` outside it.
    pub fn pixel(&self, dx: usize, dy: usize) -> Option<u8> {
        if dx >= self.w * 4 || dy >= self.h {
            return None;
        }
        let b = *self.data.get(dy * self.w + dx / 4)?;
        Some((b >> (6 - 2 * (dx % 4))) & 3)
    }
}

/// The mechanical name of an item code — no guessed nouns; the manual's names where it has one.
///
/// The three escape items are pinned by measurement, not by guesswork: `DS:0x2D60` maps codes
/// 9/10/11 to mask bits 6/7/9, and `profile/rois.toml` measured the kit box's cells against those
/// bits (each cell starts flashing within 0.18 s of exactly one mask edge): **bit 9 = left =
/// punch card (cyan), bit 7 = middle = fuel canister (black), bit 6 = right = gyroscope
/// (magenta)**. So code 9 = gyroscope, 10 = fuel canister, 11 = punch card. Stage 7's names had 9
/// and 11 the other way round (finding D1 of the self-test); stage 8 fixed them here and in the
/// port's own kit-box cell order, which reproduces the same mapping from the drawing code.
pub fn item_name(code: u16) -> &'static str {
    if code == CARRY_NONE {
        return "nothing";
    }
    match code {
        0 => "nothing",
        1 => "white plunger",
        2 => "black plunger",
        3 => "saw",
        4 => "white bucket",
        5 => "black bucket",
        6 => "ice pick",
        7 => "snowshoes",
        8 => "dynamite",
        9 => "gyroscope (part 9)",
        10 => "fuel canister (part 10)",
        11 => "punch card (part 11)",
        12 => "Launch Briefcase",
        13 => "spilled water (ice patch)",
        14 => "hole",
        15 => "hatch cover (closed)",
        16 => "hatch cover (open)",
        17 => "polar bear",
        20 => "icicle",
        _ => "unknown code",
    }
}

/// The seven trapulator slots' names, in cursor order (`0x2D8C`'s order, manual page 6).
pub const SLOT_NAMES: [&str; 7] = [
    "detonator",
    "saw",
    "water bucket",
    "ice pick",
    "snowshoes",
    "dynamite",
    "map",
];

/// The trapulator dot cells, measured: seven 2x2 cores per console.
///
/// `profile/rois.toml` (`trapulator` / `trapulator_p1`): the strip's seven dot cells start at
/// x = 102, 125, 147, 168, 190, 212, 234 and the lit core is the 2x2 at +1..+2; the top strip is
/// 108 rows above the bottom one, whose core rows are 183..184. So player 1 (top) sits at y 75..76
/// and player 2 at y 183..184.
pub const TRAY_DOT_X: [usize; 7] = [103, 126, 148, 169, 191, 213, 235];
pub const TRAY_DOT_Y: [usize; 2] = [75, 183];
/// Colour indices in the port's own 4-colour space: 1 = cyan ("the tool is in the trapulator"),
/// 0 = black ("absent"). The same indices the ROI reader scores.
pub const DOT_HELD: u8 = 1;
pub const DOT_ABSENT: u8 = 0;
// (The captured icon canvases marked "the box shows through" with 255; `chrome::kit_canvas`
// stamps the box's own `FF` bytes instead, so that constant went with the canvases in stage 29.)

// The per-player DS addresses the engine mirrors, so a recorded dump can seed them.
pub const DS_COST_CARRIED: usize = 0x2F2C;
pub const DS_COST_KIT: usize = 0x32A3;
pub const DS_COST_DYN: usize = 0x330F;
pub const DS_COST_CASE: usize = 0x2F8A;
pub const DS_COST_CURSOR: usize = 0x2F3C;
pub const DS_COST_OPEN: usize = 0x2F38;
pub const DS_COST_MASK: usize = 0x2EB8;
