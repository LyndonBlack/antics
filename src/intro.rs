//! The intro, as a **port of the original's own engine** (`spytitl.exe`).
//!
//! This module is a literal Rust transcription of the machine described in
//! `recon/round/port_check/STAGE27A_SPEC.md`, cross-read against the instruction stream of the
//! player's own executable. It reads nothing but the player's files:
//!
//! ```text
//! spytitl.exe   code, the 13 static frame records, the two 2 bpp mask planes, the scripts,
//!               the ten home descriptors, the two ping-pong frame tables
//! TITLE.DAT     the arctic scene (a linear 320x200, 80 bytes a row, 2 bpp image)
//! MAXX.PIC      the MAXX OUT! card
//! TEXT1..3.DAT  the three caption bands (54 rows of 80 bytes, after a 2-byte header)
//! ```
//!
//! No captured pixels are involved: every byte this module draws comes from one of those four
//! kinds of file. The old `assets/intro_{bg,pieces,occlusion,timeline}.bin` are *oracles* for the
//! tests only (see `recon/round/port_check/STAGE26A_REPORT.md` for why they had to go).
//!
//! # The machine, in one paragraph
//!
//! The scene never leaves RAM: `DS:0xC42` holds a linear 320x200 2 bpp image, and the screen is
//! only ever written by blitting into the interleaved video buffer at `B800:0000`. Ten 12-byte
//! descriptors (`DS:0x9BF0`, y *before* x) name a rect and a frame record; a six-command script
//! (`DS:0x9C96/9C9F/9CA9`) moves them one row or four pixels at a time and calls the masked
//! blitter (`CS:0x142B`) which pastes the record into the screen, blending the two masked pieces
//! against `DS:0xC42` through a real 2 bpp alpha plane. A 54-row band blitter (`CS:0x07A0`) writes
//! the captions over the top, a clip window (`CS:0x1B50`) makes the two cave creatures appear only
//! inside their apertures, and a nine-bit flag word (`DS:0x9C6A`) is the whole motion vocabulary.
//! The intro ends by itself: command 5's stage 10 sets `DS:0x9CC8` (quit).
//!
//! # Address model
//!
//! The program is one segment. `file offset = 0x200 + DS:offset`, and `CS` is `DS + 0x9D1`, so the
//! code image starts at DS image offset `0x9D10`. The `ds` array here *is* the load image (the
//! same trick the port already uses for `round_digits.bin`), so the static tables, the thirteen
//! frame records and the two mask records come from the executable for free.
//!
//! # Frame index
//!
//! A state of the machine is identified by its **dispatch count**: `frame(0)` is the card,
//! `frame(1)` the composed scene, and `frame(n)` the screen after the `n-1`-th script byte. The
//! recorder that produced `recon/timeline` polled an independent, machine-speed-dependent clock,
//! so its 654 capture indices are *not* a property of the game (the spec says so, §10.5, and the
//! lag it introduces is measured in `STAGE28A_REPORT.md`); the dispatch count is the only index
//! the player's own files can produce, and it covers the whole intro (0..=1841).

use std::fs;
use std::path::Path;

pub const W: usize = 320;
pub const H: usize = 200;

/// The video buffer is a full 64 KiB-segment page: rows 0..199 use 16,000 bytes of it, but
/// `row_off(199)` starts at 16,112, so the original writes (and the recording captured) 0x4000.
pub const VIDEO_BYTES: usize = 0x4000;
/// The scene buffer (`DS:0xC42`) and `MAXX.PIC`/`TITLE.DAT` are these many bytes.
pub const SCENE_BYTES: usize = 16000;
/// The state after the last dispatch of the self-terminating script (measured: 1,840 dispatches).
pub const LAST_STATE: usize = 1841;
/// Hard ceiling on the dispatch count, one more than the measured end. Nothing in the machine may
/// run past it, so any loop that records one state per dispatch is finite even if the quit word is
/// lost (`step` also stops at it).
pub const MAX_DISPATCH: usize = 2000;

const DS_LEN: usize = 0x9D10;
const EXE_DATA: usize = 0x200;

// ---- DS offsets, spec §2.2 (all `DS:` unless noted) ------------------------------------------
const A_TAB1: usize = 0x0001; // cmd-1 frame table, indexed phase+14
const A_TAB3: usize = 0x001B; // cmd-3 frame table, indexed phase+38
const A_PH1: usize = 0x0032; // ping-pong phase: cmd1 .. cmd4
const A_PH2: usize = 0x0034;
const A_PH3: usize = 0x0036;
const A_PH4: usize = 0x0038;
const A_SUB5: usize = 0x003A; // sub-counters of cmd5 / cmd6
const A_SUB6: usize = 0x003C;
const A_ST5: usize = 0x003E; // stage of cmd5 / cmd6 (0..10)
const A_ST6: usize = 0x0040;
const A_SOUND_ON: usize = 0x0000; // "the intro's own notes play"
const A_ROWS: usize = 0x9CB6; // rows scratch, used by the band blit and by the rotator
const A_WB: usize = 0x9CB8; // bytes-per-row scratch, same
const A_MASKPTR: usize = 0xB84; // {0x06B0, 0x0918, 0x0B80, 0x0B81}
const A_MASKID: usize = 0xB82;
const A_SCENE: usize = 0xC42; // the linear 16000-byte scene == DS:0xC42
const A_TEXT: [usize; 3] = [0x4AF3, 0x5BD5, 0x6CB7];
const A_CUTBUF: [usize; 2] = [0x7DB0, 0x806C]; // cut buffers for frames 0 and 1
const A_FRAME: usize = 0x9BA4; // current frame id
const A_FRAMEPTR: usize = 0x9BA6; // 15-word frame pointer table
const A_FLAGS: usize = 0x9C6A; // the nine-bit draw-flag word
const A_CUR: usize = 0x9C6C; // current slot index
const A_SLOTPTR: usize = 0x9C6E; // ten-word slot pointer table
const A_SCRIPT: [usize; 3] = [0x9C96, 0x9C9F, 0x9CA9];
/// The script **pointer** table (`DS:0x9CD0`): the loop reads the current script's address from
/// here, keyed by `DS:0x9CBE`. Reading a word out of `A_SCRIPT` instead would take the script's own
/// first two bytes (`01 63`) as an address.
const A_SCRIPTPTR: usize = 0x9CD0;
const A_CMD: usize = 0x9CBC; // current command byte
const A_SCRIPTN: usize = 0x9CBE; // current script index
const A_QUIT: usize = 0x9CC8;
const A_SOUND: usize = 0x9CCE; // sound request 1..4
const A_RECT: usize = 0x9CC0; // scratch rect: y1, y2, x1, x2
const A_STRIPN: usize = 0x9CF0; // caption selector, starts at 0xFFFF
const A_CLIPDELTA: usize = 0x9D00; // dy1, dy2, dx1, dx2
const A_CLIP: usize = 0x9D08; // clip rect: y1, y2, x1, x2
const NO_RECORD: usize = 0xFFFF;

/// The ten home descriptors, written by `CS:0x0990` (spec §2.4): `(slot, y1, y2, x1, x2, frame,
/// mask)`. Slots 0 and 1 are *not* here -- the entry cuts those two out of the screen itself.
const HOME: [(usize, u16, u16, u16, u16, u8, u8); 8] = [
    (2, 0x61, 0x81, 0x5A, 0xDA, 2, 0xFF),
    (3, 0x61, 0x81, 0x5A, 0xDA, 3, 0xFF),
    (4, 0x76, 0x97, 0x00, 0x47, 4, 0x00),
    (5, 0x76, 0x97, 0x00, 0x47, 5, 0x01),
    (6, 0x9B, 0xBD, 0x18, 0x43, 6, 0xFF),
    (7, 0xA0, 0xC0, 0x84, 0xBB, 9, 0xFF),
    (8, 0x00, 0x03, 0x00, 0x2F, 0x0C, 0xFF),
    (9, 0x00, 0x03, 0x00, 0x2F, 0x0D, 0xFF),
];

/// CGA row address (`CS:0x0891`, the table at `DS:0x0362`): even rows in the first half of the
/// buffer, odd rows in the second, 80 bytes each.
pub fn row_off(y: usize) -> usize {
    (y / 2) * 80 + (y % 2) * 0x2000
}

/// `CS:0x144D`: the destination byte of a pixel x, floored, signed -- `x = -4` gives byte -1, and
/// the blitter then skips one source byte through the clip's dx1. (An unsigned shift here is a
/// visible bug: the submarine's final descent stops being drawn.)
fn floor4(x: i32) -> i32 {
    if x >= 0 {
        x >> 2
    } else {
        -(((-x - 1) >> 2) + 1)
    }
}

/// The player's files the engine needs. Everything is checked as it is read: `MAXX.PIC` and
/// `TITLE.DAT` must be full screens, the captions must be 54-row bands.
struct Files {
    maxx: Vec<u8>,
    title: Vec<u8>,
    texts: [Vec<u8>; 3],
}

/// One frame of the machine: what the intro shows, with no reference to how it got there. The
/// port's CLI only needs `count`, but a test compares the whole descriptor, so the struct is kept
/// whole rather than narrowed to what today's caller happens to read.
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
pub struct SlotState {
    pub count: u16,
    pub y1: u16,
    pub y2: u16,
    pub x1: u16,
    pub x2: u16,
    pub frame: u8,
    pub mask: u8,
}

pub struct Intro {
    /// The executable's load image, kept pristine so [`Intro::seek`] can always restart.
    ds0: Vec<u8>,
    /// The working data segment (`DS:`).
    ds: Vec<u8>,
    /// The video buffer (`ES:0xB800`), in the interleaved layout the original writes.
    vram: Vec<u8>,
    files: Files,
    pos: usize,
    quit: bool,
    script: usize,
    off: usize,
}

impl Intro {
    /// Read the four kinds of player file out of `dir` (the unpacked 256-colour release).
    pub fn load(dir: &Path) -> Result<Intro, String> {
        let read = |name: &str| -> Result<Vec<u8>, String> {
            let p = dir.join(name);
            fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))
        };
        let exe = read("spytitl.exe")?;
        let maxx = read("MAXX.PIC")?;
        let title = read("TITLE.DAT")?;
        let t1 = read("TEXT1.DAT")?;
        let t2 = read("TEXT2.DAT")?;
        let t3 = read("TEXT3.DAT")?;
        Intro::from_files(&exe, &maxx, &title, [&t1, &t2, &t3])
    }

    /// The same, from buffers already in memory -- so a test can hold the player's files without
    /// a directory, exactly as it does for the other scenes.
    pub fn from_files(
        exe: &[u8],
        maxx: &[u8],
        title: &[u8],
        texts: [&[u8]; 3],
    ) -> Result<Intro, String> {
        if exe.len() < EXE_DATA + DS_LEN {
            return Err(format!(
                "spytitl.exe: {} B is too short (need {})",
                exe.len(),
                EXE_DATA + DS_LEN
            ));
        }
        // The MZ header tells us where the load image is; the port trusts it rather than the file
        // length, because the image is sparse and the code segment sits after the data.
        if exe[0] != b'M' || exe[1] != b'Z' {
            return Err("spytitl.exe: not an MZ executable".into());
        }
        let hdr = u16::from_le_bytes([exe[8], exe[9]]) as usize * 16;
        if hdr > EXE_DATA {
            return Err(format!("spytitl.exe: unexpected load offset {hdr}"));
        }
        if maxx.len() < SCENE_BYTES {
            return Err(format!("MAXX.PIC: {} B, want {SCENE_BYTES}", maxx.len()));
        }
        if title.len() < SCENE_BYTES {
            return Err(format!("TITLE.DAT: {} B, want {SCENE_BYTES}", title.len()));
        }
        for (i, t) in texts.iter().enumerate() {
            if t.len() < 2 + 54 * 80 {
                return Err(format!(
                    "TEXT{}.DAT: {} B, want {}",
                    i + 1,
                    t.len(),
                    2 + 54 * 80
                ));
            }
            if t[0] != 54 || t[1] != 80 {
                return Err(format!(
                    "TEXT{}.DAT: header {}x{} is not 54x80",
                    i + 1,
                    t[0],
                    t[1]
                ));
            }
        }
        let ds0 = exe[EXE_DATA..EXE_DATA + DS_LEN].to_vec();
        // The loop reads its script address out of the pointer table at `DS:0x9CD0`; check the
        // table names the three scripts this module documents. (Reading a script's own bytes as a
        // pointer is the mistake that made an earlier engine run script 0 for ever, so it is worth
        // refusing a build whose table says something else.)
        for (i, &want) in A_SCRIPT.iter().enumerate() {
            let got = u16::from_le_bytes([ds0[A_SCRIPTPTR + 2 * i], ds0[A_SCRIPTPTR + 2 * i + 1]]);
            if got as usize != want {
                return Err(format!(
                    "spytitl.exe: script pointer {i} is {got:#x}, expected {want:#x}"
                ));
            }
        }
        let ds = ds0.clone(); // the loader loads the image before `entry` runs: DS is the file
        Ok(Intro {
            ds0,
            ds,
            vram: vec![0; VIDEO_BYTES],
            files: Files {
                maxx: maxx[..SCENE_BYTES].to_vec(),
                title: title[..SCENE_BYTES].to_vec(),
                texts: [
                    texts[0].to_vec(),
                    texts[1].to_vec(),
                    texts[2].to_vec(),
                ],
            },
            pos: 0,
            quit: false,
            script: 0,
            off: 0,
        })
    }

    // ---- DS accessors -------------------------------------------------------------------------

    fn w(&self, a: usize) -> u16 {
        u16::from_le_bytes([self.ds[a], self.ds[a + 1]])
    }

    /// The same word read as a signed 16-bit value, which is what the arithmetic uses.
    fn sw(&self, a: usize) -> i32 {
        self.w(a) as i16 as i32
    }

    fn putw(&mut self, a: usize, v: u16) {
        let b = v.to_le_bytes();
        self.ds[a] = b[0];
        self.ds[a + 1] = b[1];
    }

    fn slot_ptr(&self, slot: usize) -> usize {
        self.w(A_SLOTPTR + slot * 2) as usize
    }

    fn frame_ptr(&self, fid: usize) -> usize {
        if fid >= 15 {
            return NO_RECORD;
        }
        self.w(A_FRAMEPTR + fid * 2) as usize
    }

    fn mask_ptr(&self, mid: usize) -> usize {
        if mid > 3 {
            return NO_RECORD;
        }
        self.w(A_MASKPTR + mid * 2) as usize
    }

    /// A record's `(rows, bytes per row)` -- stored rows first, then the width in bytes.
    fn rec_dims(&self, rec: usize) -> (i32, i32) {
        if rec == NO_RECORD {
            return (0, 0);
        }
        (self.w(rec) as i32, self.w(rec + 2) as i32)
    }

    // ---- CS:0x155A -- cut a rect of the screen into a record ---------------------------------

    fn cut(&mut self, dest: usize) -> u16 {
        let x1b = floor4(self.sw(A_RECT + 4));
        let x2b = (self.w(A_RECT + 6) >> 2) as i32;
        let wb = x2b - x1b + 1;
        let y1 = self.w(A_RECT) as i32;
        let y2 = self.w(A_RECT + 2) as i32;
        let rows = y2 - y1 + 1;
        self.putw(dest + 2, wb as u16);
        self.putw(dest, rows as u16);
        let mut p = dest + 4;
        for r in 0..rows.max(0) {
            let y = y1 + r;
            if !(0..H as i32).contains(&y) || wb <= 0 {
                p += wb.max(0) as usize;
                continue;
            }
            let src = row_off(y as usize) + x1b as usize;
            let n = wb as usize;
            if src + n <= VIDEO_BYTES {
                let (a, b) = (p, src);
                self.ds[a..a + n].copy_from_slice(&self.vram[b..b + n]);
            }
            p += n;
        }
        (rows.max(0) * wb.max(0)) as u16
    }

    // ---- CS:0x1B50 / CS:0x1BDD -- the clip window --------------------------------------------

    fn clip_ok(&mut self) -> bool {
        let (cy1, cy2) = (self.sw(A_CLIP), self.sw(A_CLIP + 2));
        let (sx1, sx2) = (self.sw(A_RECT), self.sw(A_RECT + 2));
        let (cx1, cx2) = ((self.w(A_CLIP + 4) >> 2) as i32, (self.w(A_CLIP + 6) >> 2) as i32);
        let (bx1, bx2) = (self.sw(0x9CB2), self.sw(0x9CB4));
        if cy1 > sx2 || cy2 < sx1 || cx1 > bx2 || cx2 < bx1 {
            return false;
        }
        for i in 0..4 {
            self.putw(A_CLIPDELTA + i * 2, 0);
        }
        if sx1 < cy1 {
            self.putw(A_CLIPDELTA, (cy1 - sx1) as u16);
        }
        if cy2 < sx2 {
            self.putw(A_CLIPDELTA + 2, (sx2 - cy2) as u16);
        }
        if bx1 < cx1 {
            self.putw(A_CLIPDELTA + 4, (cx1 - bx1) as u16);
        }
        if cx2 < bx2 {
            self.putw(A_CLIPDELTA + 6, (bx2 - cx2) as u16);
        }
        true
    }

    fn clip_reset(&mut self) {
        self.putw(A_CLIP, 0);
        self.putw(A_CLIP + 2, 199);
        self.putw(A_CLIP + 4, 0);
        self.putw(A_CLIP + 6, 0x13F);
    }

    // ---- CS:0x142B -- the blitter ------------------------------------------------------------

    /// Paste `rec` (a frame record) at the scratch rect, through `mask` if it is not `NO_RECORD`.
    ///
    /// The two masked pieces are not stencilled: the mask is a 2 bpp alpha plane (`0` takes the
    /// background, which is `DS:0xC42` -- `TITLE.DAT`, never the screen), so a byte of mask blends
    /// four pixels independently.
    fn blit(&mut self, rec: usize, mask: usize) -> bool {
        if rec == NO_RECORD {
            return false;
        }
        let (rows0, wb0) = self.rec_dims(rec);
        let y1 = self.w(A_RECT) as i32;
        self.putw(A_RECT + 2, (y1 + rows0 - 1) as u16);
        self.putw(A_WB, wb0 as u16);
        let x1b = floor4(self.sw(A_RECT + 4));
        self.putw(0x9CB2, (x1b as u16) as u16);
        self.putw(0x9CB4, (x1b + wb0 - 1) as u16);
        if !self.clip_ok() {
            return false;
        }
        let dy1 = self.sw(A_CLIPDELTA);
        let dy2 = self.sw(A_CLIPDELTA + 2);
        let dx1 = self.sw(A_CLIPDELTA + 4);
        let dx2 = self.sw(A_CLIPDELTA + 6);
        let y0 = y1 + dy1;
        let rows = rows0 - dy1 - dy2;
        let xb = x1b + dx1;
        let wb = wb0 - dx1 - dx2;
        // The clip already refuses the draws that would need a negative row or byte count (the
        // `loop` instruction would otherwise run 65,536 times), so this is belt and braces.
        if rows <= 0 || wb <= 0 {
            return true;
        }
        let src0 = rec as i32 + 4 + dy1 * wb0;
        let msk0 = if mask == NO_RECORD {
            -1
        } else {
            mask as i32 + 4 + dy1 * wb0
        };
        for r in 0..rows {
            let y = y0 + r;
            if !(0..H as i32).contains(&y) {
                continue;
            }
            let dst = row_off(y as usize) as i32 + xb;
            if dst < 0 || dst + wb > VIDEO_BYTES as i32 {
                continue;
            }
            let s = src0 + r * wb0;
            let m = msk0 + r * wb0;
            if (s + wb) as usize > self.ds.len() {
                continue;
            }
            if msk0 < 0 || (m + wb) as usize > self.ds.len() {
                let (a, b) = (dst as usize, s as usize);
                let n = wb as usize;
                self.vram[a..a + n].copy_from_slice(&self.ds[b..b + n]);
            } else {
                let bg = A_SCENE as i32 + y * 80 + xb;
                if bg < 0 || bg + wb > self.ds.len() as i32 {
                    continue;
                }
                for k in 0..wb as usize {
                    let a = self.ds[s as usize + k];
                    let b = self.ds[m as usize + k];
                    let g = self.ds[bg as usize + k];
                    self.vram[dst as usize + k] = (a & b) | (g & !b);
                }
            }
        }
        true
    }

    // ---- CS:0x165B -- build a descriptor (no flag handling) ----------------------------------

    fn build(&mut self) {
        let slot = self.w(A_CUR) as usize;
        let si = self.slot_ptr(slot);
        let rec = self.frame_ptr(self.w(A_FRAME) as usize);
        let count = if self.w(A_FLAGS) & 1 != 0 {
            self.cut(rec)
        } else {
            let (rows, wb) = self.rec_dims(rec);
            ((rows * wb) & 0xFFFF) as u16
        };
        self.putw(si, count);
        self.putw(si + 2, self.w(A_RECT));
        self.putw(si + 4, self.w(A_RECT + 2));
        self.putw(si + 6, self.w(A_RECT + 4));
        self.putw(si + 8, self.w(A_RECT + 6));
        self.ds[si + 10] = self.w(A_FRAME) as u8;
        self.ds[si + 11] = self.w(A_MASKID) as u8;
    }

    // ---- CS:0x0C60 -- rotate a record one pixel right, destructively, in place --------------

    /// The rotator reads its shape from `DS:0x9CB6`/`DS:0x9CB8` (rows / bytes per row as the
    /// drawer just computed them), not from the record header -- which is a distinction without a
    /// difference for every slot that ever rotates, and is kept because it is what the code does.
    fn rotate(&mut self, off: usize) {
        let rows = self.w(A_ROWS) as usize;
        let wb = self.w(A_WB) as usize;
        if wb == 0 {
            return;
        }
        for r in 0..rows {
            let base = off + r * wb;
            if base + wb > self.ds.len() {
                return;
            }
            let mut carry = 0u8;
            for i in (0..wb).rev() {
                let b = self.ds[base + i];
                self.ds[base + i] = (b >> 2) | (carry << 6);
                carry = b & 3;
            }
        }
    }

    // ---- CS:0x16B2 -- draw one descriptor ----------------------------------------------------

    fn draw(&mut self) -> bool {
        let slot = self.w(A_CUR) as usize;
        let si = self.slot_ptr(slot);
        self.putw(A_FRAME, self.ds[si + 10] as u16);
        let rec = self.frame_ptr(self.w(A_FRAME) as usize);
        let mid = self.ds[si + 11];
        self.putw(A_MASKID, mid as u16);
        let mask = if mid == 0xFF {
            NO_RECORD
        } else {
            self.mask_ptr(mid as usize)
        };

        let mut y1 = self.sw(si + 2);
        let mut y2 = self.sw(si + 4);
        self.putw(A_ROWS, (y2 - y1 + 1) as u16);
        let flags = self.w(A_FLAGS);
        if flags & 1 != 0 {
            y1 -= 1;
            y2 -= 1;
            self.putw(si + 2, y1 as u16);
            self.putw(si + 4, y2 as u16);
        } else if flags & 2 != 0 {
            y1 += 1;
            y2 += 1;
            self.putw(si + 2, y1 as u16);
            self.putw(si + 4, y2 as u16);
        } else if flags & 0x10 != 0 {
            let span = y2 - y1;
            y1 = self.sw(A_RECT);
            y2 = y1 + span;
            self.putw(si + 2, y1 as u16);
            self.putw(si + 4, y2 as u16);
        }

        let mut x1 = self.sw(si + 6);
        let mut x2 = self.sw(si + 8);
        let wb = ((self.w(si + 8) & 0xFFFC).wrapping_sub(self.w(si + 6) & 0xFFFC) >> 2).wrapping_add(1);
        self.putw(A_WB, wb);
        if flags & 4 != 0 {
            x1 -= 4;
            x2 -= 4;
            self.putw(si + 6, x1 as u16);
            self.putw(si + 8, x2 as u16);
        } else if flags & 8 != 0 {
            x1 += 4;
            x2 += 4;
            self.putw(si + 6, x1 as u16);
            self.putw(si + 8, x2 as u16);
        } else if flags & 0x20 != 0 {
            // Snap the caller's rect to the 4-pixel grid, keeping the slot's width.
            let snap = self.sw(A_RECT + 4) & !3;
            self.putw(A_RECT + 4, snap as u16);
            if x1 < snap {
                let d = snap - x1;
                x1 = snap;
                x2 += d;
            } else {
                let d = x1 - snap;
                x1 = snap;
                x2 -= d;
            }
            self.putw(si + 6, x1 as u16);
            self.putw(si + 8, x2 as u16);
        }

        if flags & 0x80 != 0 {
            if rec != NO_RECORD {
                self.rotate(rec + 4);
            }
            if mask != NO_RECORD {
                self.rotate(mask + 4);
            }
        }
        if flags & 0x40 != 0 {
            return false;
        }
        self.putw(A_RECT, y1 as u16);
        self.putw(A_RECT + 2, y2 as u16);
        self.putw(A_RECT + 4, x1 as u16);
        self.putw(A_RECT + 6, x2 as u16);
        self.blit(rec, mask)
    }

    // ---- CS:0x07A0 -- the band blitter -------------------------------------------------------

    /// A straight 54-row copy for all four of the sources the intro uses (the scene's own top and
    /// the three captions): the recording shows the original's scrambled column walk leaves no
    /// trace in the output (spec §4.4, `p27a_band.py`).
    fn band(&mut self, src: usize, dst_y: usize) {
        let rows = self.w(A_ROWS) as usize;
        let wb = (self.w(A_WB) as usize).min(80);
        for r in 0..rows {
            let y = dst_y + r;
            if y >= H {
                break;
            }
            let d = row_off(y);
            let s = src + r * self.w(A_WB) as usize;
            if s + wb > self.ds.len() {
                break;
            }
            self.vram[d..d + wb].copy_from_slice(&self.ds[s..s + wb]);
        }
    }

    // ---- CS:0x0850 / CS:0x1950 -- the two full-screen writes ---------------------------------

    fn push_screen(&mut self, src: usize) {
        for y in 0..H {
            let d = row_off(y);
            let s = src + y * 80;
            self.vram[d..d + 80].copy_from_slice(&self.ds[s..s + 80]);
        }
    }

    /// The wipe the arctic scene rises through: three fills in *plane* space, not row space.
    fn ramp(&mut self) {
        self.vram[0x0000..0x0F00].fill(0xDD);
        self.vram[0x2000..0x2F00].fill(0x77);
        self.vram[0x0F00..0x1F40].fill(0x55);
        self.vram[0x2F00..0x3F40].fill(0x55);
    }

    // ---- CS:0x1AA6 -- the caption selector ---------------------------------------------------

    fn strip_select(&mut self) {
        match self.w(A_STRIPN) {
            n @ (0 | 2 | 4) => {
                let src = A_TEXT[(n / 2) as usize];
                self.putw(A_ROWS, 54);
                self.putw(A_WB, 80);
                self.band(src + 2, 0);
            }
            1 | 3 => {
                self.putw(A_ROWS, 54);
                self.putw(A_WB, 80);
                self.band(A_SCENE, 0);
            }
            5 => self.putw(A_SCRIPTN, 1),
            _ => {}
        }
    }

    // ---- CS:0x0990 -- init -------------------------------------------------------------------

    fn init(&mut self) {
        self.putw(A_SUB6, 0);
        self.putw(A_ST6, 0);
        self.putw(A_SOUND, 0);
        self.putw(A_PH1, 9);
        self.putw(A_PH2, 0);
        self.putw(A_PH3, 4);
        self.putw(A_PH4, 0);
        self.ds[A_SOUND_ON] = 1;
        self.putw(A_SCRIPTN, 0);
        self.putw(A_QUIT, 0);
        self.putw(A_STRIPN, 0xFFFF);
        self.putw(A_SUB5, 0);
        self.putw(A_ST5, 0);
        self.clip_reset();
        for &(slot, y1, y2, x1, x2, fid, mid) in HOME.iter() {
            self.putw(A_RECT, y1);
            self.putw(A_RECT + 2, y2);
            self.putw(A_RECT + 4, x1);
            self.putw(A_RECT + 6, x2);
            self.putw(A_CUR, slot as u16);
            self.putw(A_FRAME, fid as u16);
            self.putw(A_MASKID, mid as u16);
            self.putw(A_FLAGS, 0);
            self.build();
        }
    }

    // ---- the six steppers --------------------------------------------------------------------

    /// A ping-pong phase counter: `+1`, and on overflow jump to `-(modulus-1)`.
    fn phase(&mut self, at: usize, top: i32) -> i32 {
        let mut c = self.sw(at) + 1;
        if c > top {
            c = -(top + 1);
        }
        self.putw(at, c as u16);
        c
    }

    /// `CS:0x008A`, slot 6: the bobbing penguin. Its frame comes from the table at `DS:0x0001`.
    fn cmd1(&mut self) {
        self.putw(A_CUR, 6);
        let c = self.phase(A_PH1, 12);
        self.putw(A_FLAGS, if (0..=12).contains(&c) { 1 } else { 2 });
        let si = self.slot_ptr(6);
        let b = self.ds[A_TAB1 + (c + 13) as usize];
        self.ds[si + 10] = b % 100;
        if b >= 100 && self.ds[A_SOUND_ON] != 0 {
            self.putw(A_SOUND, 2);
        }
        self.draw();
    }

    /// `CS:0x00EC`, slot 0: the left cut piece -- and the caption driver.
    fn cmd2(&mut self) {
        self.putw(A_CUR, 0);
        let c = self.phase(A_PH2, 6);
        if (0..=6).contains(&c) {
            if c == 0 {
                let n = self.w(A_STRIPN).wrapping_add(1);
                self.putw(A_STRIPN, n);
                self.strip_select();
            }
            self.putw(A_FLAGS, 1);
        } else {
            self.putw(A_FLAGS, 2);
        }
        self.draw();
    }

    /// `CS:0x0129`, slot 7: the right penguin, three frames from the table at `DS:0x001B`.
    fn cmd3(&mut self) {
        self.putw(A_CUR, 7);
        let c = self.phase(A_PH3, 10);
        self.putw(A_FLAGS, if c >= 0 { 1 } else { 2 });
        let si = self.slot_ptr(7);
        let b = self.ds[A_TAB3 + (c + 11) as usize];
        self.ds[si + 10] = b % 100;
        if b >= 100 && self.ds[A_SOUND_ON] != 0 {
            self.putw(A_SOUND, 2);
        }
        self.draw();
    }

    /// `CS:0x018B`, slot 1: the right cut piece.
    fn cmd4(&mut self) {
        self.putw(A_CUR, 1);
        let c = self.phase(A_PH4, 6);
        self.putw(A_FLAGS, if c >= 0 { 1 } else { 2 });
        self.draw();
    }

    /// `CS:0x01BB`, slot 4: the left cave creature, a ten-stage machine.
    fn cmd5(&mut self) {
        self.putw(A_CUR, 4);
        let st = self.w(A_ST5);
        let mut sub = self.w(A_SUB5);
        match st {
            0 => {
                self.putw(A_RECT, 0x94);
                self.putw(A_RECT + 4, 0x00);
                self.putw(A_FLAGS, 0x70);
                self.draw();
                self.putw(A_ST5, 1);
                self.putw(A_SUB5, 0);
            }
            1 => {
                self.putw(A_CLIP, 0x74);
                self.putw(A_CLIP + 2, 0x92);
                self.putw(A_CLIP + 4, 0x00);
                self.putw(A_CLIP + 6, 0x48);
                sub = sub.wrapping_add(1);
                self.putw(A_SUB5, sub);
                if sub <= 0x1C {
                    self.putw(A_FLAGS, 1);
                    self.draw();
                }
                if sub == 0x28 || sub == 0x2D {
                    self.putw(A_SOUND, if sub == 0x28 { 4 } else { 3 });
                }
                if sub >= 0x38 {
                    self.putw(A_ST5, 2);
                    self.putw(A_SUB5, 0);
                }
            }
            2 => {
                self.putw(A_FLAGS, if sub <= 1 { 1 } else { 9 });
                self.draw();
                self.putw(A_FLAGS, 1);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB5, sub);
                if sub >= 9 {
                    self.putw(A_ST5, 3);
                    self.putw(A_SUB5, 0);
                }
            }
            3 => {
                self.putw(A_CLIP + 6, 0x78);
                self.putw(A_FLAGS, 8);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB5, sub);
                if sub >= 0x19 {
                    self.putw(A_ST5, 4);
                    self.putw(A_SUB5, 0);
                }
            }
            4 => {
                self.putw(A_FLAGS, 0xC0);
                self.draw();
                self.putw(A_ST5, 5);
            }
            5 => {
                self.putw(A_SCRIPTN, 2);
                self.blink();
                self.putw(A_SUB5, 0);
                self.putw(A_ST5, 6);
            }
            6 => {
                self.putw(A_FLAGS, 4);
                self.putw(A_CLIP + 6, 0x78);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB5, sub);
                if sub >= 0x1A {
                    self.putw(A_ST5, 8);
                    self.putw(A_SUB5, 0);
                }
            }
            8 => {
                self.putw(A_FLAGS, if sub < 7 { 6 } else { 2 });
                self.draw();
                self.putw(A_FLAGS, 2);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB5, sub);
                if sub >= 9 {
                    self.putw(A_ST5, 9);
                    self.putw(A_SUB5, 0);
                }
            }
            9 => {
                self.putw(A_CLIP + 2, 0x92);
                self.putw(A_CLIP + 4, 0x00);
                self.putw(A_CLIP + 6, 0x48);
                self.putw(A_FLAGS, 2);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB5, sub);
                if sub >= 0x1C {
                    self.putw(A_ST5, 10);
                    self.putw(A_SUB5, 0);
                }
            }
            10 => {
                sub = sub.wrapping_add(1);
                self.putw(A_SUB5, sub);
                if sub >= 0x14 {
                    self.putw(A_QUIT, 1);
                }
            }
            _ => {}
        }
        self.clip_reset();
        self.extra5();
    }

    /// `CS:0x03B5`: the sliver (slot 8) that rides on the left creature.
    fn extra5(&mut self) {
        let st = self.w(A_ST5);
        if st == 0 || st == 4 || st == 5 || st > 9 {
            self.clip_reset();
            return;
        }
        let si = self.slot_ptr(4);
        let sj = self.slot_ptr(8);
        self.putw(A_CUR, 8); // the lookup leaves the index at slot 8
        self.putw(A_CLIP, 0x00);
        self.putw(A_CLIP + 2, 0x9C);
        self.putw(A_CLIP + 4, 0x00);
        self.putw(A_CLIP + 6, 0x64);
        let sub = self.w(A_SUB5);
        let mut al;
        if st == 1 {
            if sub < 4 {
                self.clip_reset();
                return;
            }
            self.putw(A_RECT, 0x94);
            self.putw(A_RECT + 4, self.w(si + 6));
            al = self.ds[sj + 10].wrapping_add(1);
            if al > 13 {
                al = 12;
            }
        } else if st <= 8 {
            self.putw(A_RECT, self.w(si + 4) - 6);
            self.putw(A_RECT + 4, self.w(si + 6));
            al = self.ds[sj + 10].wrapping_add(1);
            if al > 13 {
                al = 12;
            }
        } else if sub <= 0x12 {
            self.putw(A_RECT, 0x94);
            self.putw(A_RECT + 4, self.w(si + 6));
            al = self.ds[sj + 10].wrapping_add(1);
            if al > 13 {
                al = 12;
            }
        } else if sub == 0x13 {
            self.putw(A_RECT, 0x94);
            self.putw(A_RECT + 4, self.w(si + 6));
            al = 14;
        } else {
            self.clip_reset();
            return;
        }
        self.ds[sj + 10] = al;
        self.putw(A_FLAGS, 0x30);
        self.draw();
        self.clip_reset();
    }

    /// `CS:0x049D`, slot 5: the right cave creature -- cmd5's mirror image.
    fn cmd6(&mut self) {
        self.putw(A_CUR, 5);
        let st = self.w(A_ST6);
        let mut sub = self.w(A_SUB6);
        match st {
            0 => {
                self.putw(A_RECT, 0x94);
                self.putw(A_RECT + 4, 0xE4);
                self.putw(A_FLAGS, 0x70);
                self.draw();
                self.putw(A_ST6, 1);
                self.putw(A_SUB6, 0);
            }
            1 => {
                self.putw(A_CLIP, 0x76);
                self.putw(A_CLIP + 2, 0x94);
                self.putw(A_CLIP + 4, 0xCC);
                self.putw(A_CLIP + 6, 0x124);
                sub = sub.wrapping_add(1);
                self.putw(A_SUB6, sub);
                if sub <= 0x1B {
                    self.putw(A_FLAGS, 1);
                    self.draw();
                }
                if sub == 0x1E {
                    self.ds[A_SOUND_ON] = 0;
                    self.putw(A_SOUND, 3);
                }
                if sub >= 0x37 {
                    self.ds[A_SOUND_ON] = 1;
                    self.putw(A_ST6, 2);
                    self.putw(A_SUB6, 0);
                }
            }
            2 => {
                self.putw(A_FLAGS, 0x05);
                self.draw();
                self.putw(A_FLAGS, 0x01);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB6, sub);
                if sub >= 7 {
                    self.putw(A_ST6, 3);
                    self.putw(A_SUB6, 0);
                }
            }
            3 => {
                self.putw(A_CLIP + 4, 0xB0);
                self.putw(A_FLAGS, 4);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB6, sub);
                if sub >= 0x1A {
                    self.putw(A_ST6, 4);
                    self.putw(A_SUB6, 0);
                }
            }
            4 => {
                self.putw(A_FLAGS, 0xC0);
                self.draw();
                self.putw(A_ST6, 5);
            }
            5 => {
                self.putw(A_ST6, 6);
                self.putw(A_SUB6, 0);
            }
            6 => {
                self.putw(A_CLIP + 4, 0xB0);
                self.putw(A_FLAGS, 8);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB6, sub);
                if sub >= 0x19 {
                    self.putw(A_ST6, 8);
                    self.putw(A_SUB6, 0);
                }
            }
            8 => {
                self.putw(A_FLAGS, 0x0A);
                self.draw();
                self.putw(A_FLAGS, 0x02);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB6, sub);
                if sub >= 7 {
                    self.putw(A_ST6, 9);
                    self.putw(A_SUB6, 0);
                }
            }
            9 | 10 => {
                // `CS:0x0622` is the cmp chain's *fall-through* (`cmp ax,8 / jne 0x622`), not a
                // stage-9 arm: every value that is not 0..8 lands here, so stage 10 keeps drawing
                // and stepping exactly as stage 9 does -- there is no stage-10 case for cmd 6 in
                // the instruction stream (only cmd 5 has one, and there it sets the quit word).
                self.putw(A_CLIP + 2, 0x94);
                self.putw(A_CLIP + 4, 0xC4);
                self.putw(A_CLIP + 6, 0x124);
                self.putw(A_FLAGS, 2);
                self.draw();
                sub = sub.wrapping_add(1);
                self.putw(A_SUB6, sub);
                if sub >= 0x1C {
                    self.putw(A_ST6, 10);
                    self.putw(A_SUB6, 0);
                }
            }
            _ => {}
        }
        self.clip_reset();
        self.extra6();
    }

    /// `CS:0x065B`: the sliver (slot 9) that rides on the right creature.
    fn extra6(&mut self) {
        let st = self.w(A_ST6);
        if st == 0 || st == 4 || st == 5 || st > 9 {
            self.clip_reset();
            return;
        }
        let si = self.slot_ptr(5);
        let sj = self.slot_ptr(9);
        self.putw(A_CUR, 9);
        self.putw(A_CLIP, 0x00);
        self.putw(A_CLIP + 2, 0x9E);
        self.putw(A_CLIP + 4, 0xB4);
        self.putw(A_CLIP + 6, 0x13F);
        let sub = self.w(A_SUB6);
        if st == 1 {
            if sub < 4 {
                self.clip_reset();
                return;
            }
            self.putw(A_RECT, 0x96);
            self.putw(A_RECT + 4, self.w(si + 6));
        } else {
            if st == 3 || st == 6 {
                self.putw(A_CLIP + 2, 0x89);
            }
            if st <= 8 {
                self.putw(A_RECT, self.w(si + 4) - 6);
                self.putw(A_RECT + 4, self.w(si + 6));
            } else if sub <= 0x13 {
                self.putw(A_RECT, 0x96);
                self.putw(A_RECT + 4, self.w(si + 6));
            } else if sub == 0x14 {
                self.putw(A_RECT, 0x96);
                self.putw(A_RECT + 4, self.w(si + 6));
                self.ds[sj + 10] = 14;
                self.putw(A_FLAGS, 0x30);
                self.draw();
                self.clip_reset();
                return;
            } else {
                self.clip_reset();
                return;
            }
        }
        let mut al = self.ds[sj + 10].wrapping_add(1);
        if al > 13 {
            al = 12;
        }
        self.ds[sj + 10] = al;
        self.putw(A_FLAGS, 0x30);
        self.draw();
        self.clip_reset();
    }

    /// `CS:0x075F`: the penguin swapping between its two frames in place, six times.
    fn blink(&mut self) {
        self.clip_reset();
        self.putw(A_SOUND, 1);
        for _ in 0..6 {
            self.putw(A_CUR, 3);
            self.putw(A_FLAGS, 0);
            self.draw();
            self.putw(A_CUR, 2);
            self.draw();
        }
    }

    // ---- CS:0x0000 -- one dispatch -----------------------------------------------------------

    fn dispatch(&mut self, c: u8) {
        self.putw(A_CMD, c as u16);
        match c {
            1 => self.cmd1(),
            2 => self.cmd2(),
            3 => self.cmd3(),
            4 => self.cmd4(),
            5 => self.cmd5(),
            6 => self.cmd6(),
            _ => {}
        }
        if self.w(A_SOUND) != 0 {
            self.putw(A_SOUND, 0);
        }
    }

    // ---- the run --------------------------------------------------------------------------

    /// `entry` up to the card: init, load `maxx.pic` into the scene buffer, push it.
    fn boot_card(&mut self) {
        self.ds.copy_from_slice(&self.ds0);
        self.vram.fill(0);
        self.pos = 0;
        self.quit = false;
        self.script = 0;
        self.off = 0;
        self.init();
        self.ds[A_SCENE..A_SCENE + SCENE_BYTES].copy_from_slice(&self.files.maxx);
        self.push_screen(A_SCENE);
    }

    /// `entry` up to the first descriptor state: load `title.dat` and the captions, wipe, band
    /// the scene's rows 54..199 in, then cut descriptors 0 and 1 out of the screen.
    fn boot_scene(&mut self) {
        self.ds[A_SCENE..A_SCENE + SCENE_BYTES].copy_from_slice(&self.files.title);
        for (i, a) in A_TEXT.iter().enumerate() {
            let d = &self.files.texts[i];
            self.ds[*a..*a + d.len()].copy_from_slice(d);
        }
        self.ramp();
        self.putw(A_ROWS, 0x92);
        self.putw(A_WB, 0x50);
        self.band(A_SCENE + 0x10E0, 0x36);
        for &(slot, x1, x2, fid) in &[(0usize, 0x4Cu16, 0x80u16, 0u8), (1, 0xE0, 0x114, 1)] {
            self.putw(A_RECT, 0xA6);
            self.putw(A_RECT + 2, 0xC1);
            self.putw(A_RECT + 4, x1);
            self.putw(A_RECT + 6, x2);
            self.putw(A_CUR, slot as u16);
            self.putw(A_FRAME, fid as u16);
            self.putw(A_MASKID, 0xFF);
            self.putw(A_FLAGS, 1);
            let rec = self.frame_ptr(fid as usize);
            self.putw(A_CUTBUF[slot], rec as u16);
            self.build();
        }
    }

    /// Run the script machine one dispatch. Returns false once the intro has ended.
    ///
    /// The end is the original's own: command 5's stage 10 counts its sub-counter to 20 and sets
    /// `DS:0x9CC8` (spec §3.3, the last row of the table). `entry` tests that byte after every
    /// dispatch (`CS:0x1920..0x192C`) and tears down at `CS:0x192E`, so the machine stops itself
    /// at dispatch 1,840. Reading `DS:0x9CC8` here -- rather than a private flag -- is the whole
    /// termination: without it the loop re-reads `DS:0x9CBE` (still 2) at the last script's `00`
    /// byte and starts script 2 again, forever.
    ///
    /// `MAX_DISPATCH` is a second, independent stop: if a future edit ever loses the quit word,
    /// the stream is still finite and a test cannot grow a buffer without bound.
    pub fn step(&mut self) -> bool {
        if self.quit || self.w(A_QUIT) != 0 {
            self.quit = true;
            return false;
        }
        if self.pos >= MAX_DISPATCH {
            self.quit = true;
            return false;
        }
        // A `00` byte ends a script and the index is re-read from `DS:0x9CBE`, which a stepper may
        // have changed (cmd 5 stage 5 -> script 2, the caption selector -> script 1). Three scripts
        // plus the hand-over is the most this can walk in one step; a script of zeroes stops here.
        for _ in 0..4 {
            let ptr = self.w(A_SCRIPTPTR + 2 * self.script.min(2)) as usize;
            let c = self.ds[ptr + self.off];
            if c == 0 {
                self.off = 0;
                self.script = (self.w(A_SCRIPTN) as usize).min(2);
                continue;
            }
            self.off += 1;
            self.dispatch(c);
            self.pos += 1;
            if self.w(A_QUIT) != 0 {
                self.quit = true;
                return false;
            }
            return true;
        }
        self.quit = true;
        false
    }

    /// Put the machine in the state `frame` (0 = the card, 1 = the composed scene, `n` = after
    /// dispatch `n-1`). Restarting is always allowed, so this is deterministic for any argument.
    pub fn seek(&mut self, frame: usize) -> usize {
        let frame = frame.min(LAST_STATE);
        self.boot_card();
        if frame == 0 {
            return self.pos;
        }
        self.boot_scene();
        self.pos = 1;
        while self.pos < frame {
            if !self.step() {
                break;
            }
        }
        self.pos
    }

    /// `0` while the card is up (`DS:0x9BD0` = 0), `1` for the arctic scene.
    pub fn palette(&self) -> u8 {
        if self.pos == 0 {
            0
        } else {
            1
        }
    }

    /// The caption band on screen: 0 none, 1..3 `TEXTn.DAT`. It is derived from the `DS:0x9CF0`
    /// selector (spec §4.7) rather than measured, but the selector is a counter, not an index: it
    /// walks -1, 0, 1, 2 ... 14 and the selector only *draws* on 0..4 (`0` text1, `1` scene, `2`
    /// text2, `3` scene, `4` text3, `5` hand over to script 1). Everything above 4 is a no-op, so
    /// the band keeps the last thing drawn -- text3 -- which is why 376 of the 653 recorded arctic
    /// screens carry TEXT3's payload and only 55 / 56 carry TEXT1 / TEXT2.
    pub fn strip(&self) -> u8 {
        match self.w(A_STRIPN) {
            0xFFFF => 0, // the init value (-1): nothing has been drawn yet
            0 => 1,
            1 | 3 => 0, // the scene's own top, caption removed
            2 => 2,
            _ => 3, // 4 drew text3; 5..14 are no-ops, so text3 stays up
        }
    }

    /// How many descriptors are live -- the port's `--state` JSON reports this.
    pub fn slot_count(&self) -> usize {
        self.slots().iter().filter(|s| s.count != 0).count()
    }

    /// The ten descriptors, as the original holds them.
    pub fn slots(&self) -> [SlotState; 10] {
        let mut out = [SlotState::default(); 10];
        for (s, o) in out.iter_mut().enumerate() {
            let si = self.slot_ptr(s);
            *o = SlotState {
                count: self.w(si),
                y1: self.w(si + 2),
                y2: self.w(si + 4),
                x1: self.w(si + 6),
                x2: self.w(si + 8),
                frame: self.ds[si + 10],
                mask: self.ds[si + 11],
            };
        }
        out
    }

    /// The video buffer as the original left it: CGA interleaved, `VIDEO_BYTES` long.
    pub fn video(&self) -> &[u8] {
        &self.vram
    }

    /// The screen as 320x200 palette indices, de-interleaved through `row_off`.
    pub fn indices(&self) -> Vec<u8> {
        let mut idx = vec![0u8; W * H];
        let vram = self.video();
        for y in 0..H {
            let o = row_off(y);
            for xb in 0..80 {
                let b = vram[o + xb];
                let d = y * W + xb * 4;
                idx[d] = (b >> 6) & 3;
                idx[d + 1] = (b >> 4) & 3;
                idx[d + 2] = (b >> 2) & 3;
                idx[d + 3] = b & 3;
            }
        }
        idx
    }
}

// ---------------------------------------------------------------------------------------------
// tests -- oracles only; every one of them stands down when the original is not on this machine
// ---------------------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::orig;

    fn intro() -> Option<Intro> {
        if orig::absent() {
            orig::skip("spytitl.exe and the intro's data files");
            return None;
        }
        let dir = orig::repo().join(orig::ORIGINAL_DIR);
        match Intro::load(&dir) {
            Ok(i) => Some(i),
            Err(e) => panic!("cannot load the intro engine from {}: {e}", dir.display()),
        }
    }

    /// The recorded screens, one per capture (`recon/timeline/frames.bin`, 0x4000 B each).
    fn frames() -> Option<Vec<u8>> {
        match orig::file("recon/timeline/frames.bin") {
            Some(p) => Some(fs::read(&p).expect("frames.bin")),
            None => {
                orig::skip("recon/timeline/frames.bin");
                None
            }
        }
    }

    /// The descriptor table of each capture, read from `meta.json` without a JSON dependency:
    /// the file is one object per capture and the only fields this test needs are `desc` (a hex
    /// string of the 120-byte table) and `palette`.
    fn captures() -> Option<Vec<Vec<(u8, i16, i16)>>> {
        let p = orig::file("recon/timeline/meta.json")?;
        let text = fs::read_to_string(&p).expect("meta.json");
        let mut out = Vec::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("\"desc\"") {
            rest = &rest[i + 6..];
            let Some(q) = rest.find('"') else { break };
            rest = &rest[q + 1..];
            let Some(end) = rest.find('"') else { break };
            let hex = &rest[..end];
            rest = &rest[end + 1..];
            let mut b = Vec::with_capacity(120);
            for k in (0..hex.len().min(240)).step_by(2) {
                b.push(u8::from_str_radix(&hex[k..k + 2], 16).expect("hex"));
            }
            out.push(visible_slots(&b));
        }
        Some(out)
    }

    /// The capture's own filter (`p27a_compare.captured_states`): a slot counts when it has a
    /// count and a rect that is inside the screen. The state is `(frame, x1, y1)` per slot,
    /// because those are the three values the recording fixes independently of the drawing.
    fn visible_slots(b: &[u8]) -> Vec<(u8, i16, i16)> {
        let mut out = Vec::new();
        for s in 0..10 {
            let d = &b[s * 12..(s + 1) * 12];
            let u = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
            let (cnt, y1, y2, x1, x2) = (u(0), u(2), u(4), u(6), u(8));
            if cnt == 0 {
                continue;
            }
            let (x1, x2) = (x1 as i16, x2 as i16);
            if (1..=320).contains(&(x2 as i32 - x1 as i32 + 1))
                && (1..=200).contains(&(y2 as i32 - y1 as i32 + 1))
            {
                out.push((d[10], x1, y1 as i16));
            }
        }
        out
    }

    fn sim_slots(i: &Intro) -> Vec<(u8, i16, i16)> {
        let mut b = Vec::with_capacity(120);
        for s in i.slots() {
            b.extend_from_slice(&s.count.to_le_bytes());
            b.extend_from_slice(&s.y1.to_le_bytes());
            b.extend_from_slice(&s.y2.to_le_bytes());
            b.extend_from_slice(&s.x1.to_le_bytes());
            b.extend_from_slice(&s.x2.to_le_bytes());
            b.push(s.frame);
            b.push(s.mask);
        }
        visible_slots(&b)
    }

    /// Longest common subsequence of two state streams; returns the index pairs, in order.
    ///
    /// One `u16` per cell (the answer cannot exceed 654, so no truncation): 654 x 1,842 cells is
    /// 2.4 MB. The backtrack must compare `table[i-1][j]` with `table[i][j-1]` -- comparing two
    /// rows of the same column always says "go up", which matches only against the final state and
    /// silently reports an LCS of 0.
    fn lcs(a: &[Vec<(u8, i16, i16)>], b: &[Vec<(u8, i16, i16)>]) -> Vec<(usize, usize)> {
        let (n, m) = (a.len(), b.len());
        let mut table = vec![vec![0u16; m + 1]; n + 1];
        for i in 1..=n {
            for j in 1..=m {
                table[i][j] = if a[i - 1] == b[j - 1] {
                    table[i - 1][j - 1] + 1
                } else {
                    table[i - 1][j].max(table[i][j - 1])
                };
            }
        }
        let (mut i, mut j) = (n, m);
        let mut out = Vec::new();
        while i > 0 && j > 0 {
            if a[i - 1] == b[j - 1] {
                out.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            } else if table[i - 1][j] >= table[i][j - 1] {
                i -= 1;
            } else {
                j -= 1;
            }
        }
        out.reverse();
        out
    }

    /// The whole state stream: at most 1,842 states -- the card, the composed scene, then one per
    /// dispatch. Bounded twice over: by `LAST_STATE` (the machine's own end, cmd 5 stage 10) and by
    /// `step`'s `MAX_DISPATCH`. A state's video buffer is kept only when the caller asks for it, so
    /// the descriptor-only tests hold kilobytes rather than 30 MB.
    fn stream(mut i: Intro, keep_video: bool) -> Vec<(Vec<(u8, i16, i16)>, Option<Vec<u8>>)> {
        let snap = |i: &Intro| {
            (
                sim_slots(i),
                if keep_video {
                    Some(i.video().to_vec())
                } else {
                    None
                },
            )
        };
        let mut out: Vec<(Vec<(u8, i16, i16)>, Option<Vec<u8>>)> =
            Vec::with_capacity(LAST_STATE + 1);
        i.boot_card(); // `entry`: init, maxx.pic into the scene buffer, push it -- the card state
        out.push(snap(&i));
        i.boot_scene(); // title.dat + the captions, the ramp wipe, the band, the two cuts
        i.pos = 1;
        out.push(snap(&i));
        while i.pos < LAST_STATE {
            let before = i.pos;
            let cont = i.step();
            if i.pos == before {
                assert!(!cont, "step() returned true without dispatching");
                break;
            }
            out.push(snap(&i));
            if !cont {
                break;
            }
        }
        out
    }

    fn differing(want: &[u8], got: &[u8]) -> usize {
        want.iter().zip(got).filter(|(a, b)| a != b).count()
    }

    fn percentile(sorted: &[usize], p: f64) -> usize {
        if sorted.is_empty() {
            return 0;
        }
        sorted[((sorted.len() as f64 - 1.0) * p) as usize]
    }

    /// **The card is `MAXX.PIC`.** Frame 0 is the player's own file pushed by `CS:0x0850`: no
    /// asset, no fit. This is the one frame the old path already got right, and it must stay at
    /// zero differing bytes.
    #[test]
    fn the_card_is_maxx_pic_decoded() {
        let Some(mut i) = intro() else { return };
        i.seek(0);
        assert_eq!(i.palette(), 0, "frame 0 is the card (palette 0)");
        assert_eq!(i.strip(), 0, "no caption over the card");
        let idx = i.indices();
        let maxx = fs::read(orig::repo().join(orig::ORIGINAL_DIR).join("MAXX.PIC")).unwrap();
        let mut want = vec![0u8; W * H];
        for (k, b) in maxx.iter().take(W * H / 4).enumerate() {
            want[k * 4] = (b >> 6) & 3;
            want[k * 4 + 1] = (b >> 4) & 3;
            want[k * 4 + 2] = (b >> 2) & 3;
            want[k * 4 + 3] = b & 3;
        }
        let d = want.iter().zip(&idx).filter(|(a, b)| a != b).count();
        assert_eq!(d, 0, "the card must be MAXX.PIC, pixel for pixel");
        if let Some(raw) = frames() {
            let rec = &raw[..VIDEO_BYTES];
            assert_eq!(differing(rec, i.video()), 0, "and equal to recorded frame 0");
        }
    }

    /// **The descriptor timeline comes out of the scripts.** 588 of the 654 captured states are
    /// found in the machine's own stream, in order; the other 66 are samples taken *while a
    /// dispatch was executing*, and every one of them is one slot away from a state the machine
    /// really passes through (spec §5.1, measured).
    #[test]
    fn the_descriptor_timeline_comes_out_of_the_scripts() {
        let Some(i) = intro() else { return };
        let Some(cap) = captures() else { return };
        let want: Vec<Vec<(u8, i16, i16)>> = cap;
        let got: Vec<Vec<(u8, i16, i16)>> = stream(i, false).into_iter().map(|s| s.0).collect();
        let pairs = lcs(&want, &got);
        assert!(
            pairs.len() >= 588,
            "LCS {} of {} captured descriptor states (want >= 588)",
            pairs.len(),
            want.len()
        );
        assert_eq!(want.len(), 654, "the recording is 654 settled screens");
        assert_eq!(got.len(), LAST_STATE + 1, "the machine stops itself: 1,842 states");
        assert!(
            pairs.last().map(|p| p.1) == Some(got.len() - 1),
            "the machine's last state is the last captured one"
        );
        // every unmatched capture is a mid-dispatch sample: one slot's y away from a real state
        let matched: std::collections::HashSet<usize> = pairs.iter().map(|p| p.0).collect();
        let mut worst = 0;
        let mut unmatched = 0;
        for (k, c) in want.iter().enumerate() {
            if matched.contains(&k) {
                continue;
            }
            unmatched += 1;
            let best = got
                .iter()
                .filter(|s| s.len() == c.len())
                .map(|s| s.iter().zip(c).filter(|(a, b)| a != b).count())
                .min()
                .unwrap_or(usize::MAX);
            worst = worst.max(best);
        }
        assert!(unmatched <= 66, "{unmatched} unmatched captures (want <= 66)");
        assert_eq!(worst, 1, "an unmatched capture is one slot away from a real state");
        println!(
            "LCS {} of {} captured states, {} unmatched, worst slot distance {}",
            pairs.len(),
            want.len(),
            unmatched,
            worst
        );
    }

    /// **The intro ends by itself, at dispatch 1,840, because cmd 5 stage 10 says so.** This is the
    /// regression test for the bug that made the earlier engine loop for ever: `step()` tested a
    /// private flag nothing set, and it read the script *bytes* where the script *pointer table*
    /// (`DS:0x9CD0`) belongs, so it re-ran script 0 and never reached cmd 5 at all.
    #[test]
    fn the_intro_ends_by_itself() {
        let Some(mut i) = intro() else { return };
        i.boot_card();
        i.boot_scene();
        i.pos = 1;
        let mut dispatches = 0;
        while i.step() {
            dispatches += 1;
            assert!(dispatches <= MAX_DISPATCH, "the machine is finite");
        }
        assert_eq!(dispatches + 1, 1840, "the intro is 1,840 dispatches (spec §3.2)");
        assert_eq!(i.pos, LAST_STATE, "state 1,841 is the last one");
        assert_eq!(i.w(A_QUIT), 1, "cmd 5 stage 10 sets the quit word DS:0x9CC8");
        assert_eq!((i.w(A_ST5), i.w(A_ST6)), (10, 10), "both creatures ended at stage 10");
        assert_eq!(i.w(A_SCRIPTN), 2, "it ended inside script 2");
        assert!(!i.step(), "and a stopped machine stays stopped");
        assert_eq!(i.pos, LAST_STATE, "without dispatching again");
    }

    /// **The pixels fall out of the machine.** Every aligned capture is compared byte for byte
    /// with the machine's own video buffer.
    ///
    /// Two numbers, because the recording has two layers of its own error:
    ///
    /// * at the dispatch its descriptor table names: median 137 B of 16,000, six frames exact --
    ///   the figure `STAGE27A_SPEC.md` §8 quotes;
    /// * at the *best of the six dispatches before it*: 486 of 588 byte-identical, median 0,
    ///   p90 60 B, worst 219 B. The recorder read the descriptor table and grabbed the screen a
    ///   few dispatches apart, so its screens are a little older than its descriptors
    ///   (`STAGE28A_REPORT.md` §3): almost the whole residual is that lag, not the engine.
    #[test]
    fn every_recorded_frame_matches_the_engine() {
        let Some(i) = intro() else { return };
        let Some(raw) = frames() else { return };
        let Some(cap) = captures() else { return };
        let stream = stream(i, true);
        let got: Vec<Vec<(u8, i16, i16)>> = stream.iter().map(|s| s.0.clone()).collect();
        let pairs = lcs(&cap, &got);
        assert_eq!(raw.len(), 654 * VIDEO_BYTES, "the recording is 654 screens");
        assert_eq!(stream.len(), LAST_STATE + 1, "the machine stops itself: 1,842 states");

        let mut at_j = Vec::new();
        let mut at_best = Vec::new();
        for (k, j) in &pairs {
            let want = &raw[k * VIDEO_BYTES..(k + 1) * VIDEO_BYTES];
            let vid = |jj: usize| stream[jj].1.as_deref().expect("video kept");
            at_j.push(differing(want, vid(*j)));
            let mut best = usize::MAX;
            for s in 0..=6usize {
                if let Some(jj) = j.checked_sub(s) {
                    best = best.min(differing(want, vid(jj)));
                }
            }
            at_best.push(best);
        }
        at_j.sort();
        at_best.sort();
        let exact_best = at_best.iter().filter(|n| **n == 0).count();
        assert_eq!(pairs.len(), 588, "aligned captures");
        assert!(
            at_j.iter().filter(|n| **n == 0).count() >= 5,
            "six frames are exact at the recorded dispatch: {}",
            at_j.iter().filter(|n| **n == 0).count()
        );
        assert!(
            percentile(&at_j, 0.5) <= 140,
            "median {} B (spec: 137)",
            percentile(&at_j, 0.5)
        );
        assert!(*at_j.last().unwrap() <= 380, "worst {} B", at_j.last().unwrap());
        assert!(
            exact_best >= 470,
            "{exact_best}/588 frames byte-identical at the recorder's own lag (want >= 470)"
        );
        assert_eq!(percentile(&at_best, 0.5), 0, "median at the best lag");
        assert!(
            percentile(&at_best, 0.9) <= 70,
            "p90 {} B at the best lag",
            percentile(&at_best, 0.9)
        );
        assert!(
            *at_best.last().unwrap() <= 230,
            "worst {} B at the best lag",
            at_best.last().unwrap()
        );
        println!(
            "at the recorded dispatch: {} of 588 exact, median {} B, p90 {} B, worst {} B",
            at_j.iter().filter(|n| **n == 0).count(),
            percentile(&at_j, 0.5),
            percentile(&at_j, 0.9),
            at_j.last().unwrap()
        );
        println!(
            "at the best of the six earlier dispatches: {exact_best} of 588 exact, median {} B, \
             p90 {} B, worst {} B",
            percentile(&at_best, 0.5),
            percentile(&at_best, 0.9),
            at_best.last().unwrap()
        );
    }

    /// **The fifteen pieces are the player's own records**, not crops of a screen: thirteen static
    /// records in `spytitl.exe` and two cut out of the scene at the entry rects. This is the claim
    /// that retires `intro_pieces.bin`, so it is checked against the data, not against the asset.
    ///
    /// Frames 0 and 1 are the exception the shape of the file makes obvious: their records live in
    /// the zeroed BSS at `DS:0x7DB0` / `DS:0x806C`, so before the entry cuts them their header is
    /// `0 x 0`. The cut is what gives them their shape, and after it they are exactly `TITLE.DAT`
    /// rows 166..193 -- which is why the tracked asset, sized by the descriptor rect, was 53 wide.
    #[test]
    fn the_pieces_are_the_players_own_records() {
        let mut i = match intro() {
            Some(i) => i,
            None => return,
        };
        let title = fs::read(orig::repo().join(orig::ORIGINAL_DIR).join("TITLE.DAT")).unwrap();
        // the table of spec §2.3: (rows, bytes/row) for each of the 15 frame records
        let dims = [
            (28, 14),
            (28, 14),
            (33, 33),
            (33, 33),
            (34, 18),
            (34, 18),
            (35, 11),
            (35, 11),
            (35, 11),
            (33, 14),
            (33, 14),
            (33, 14),
            (4, 18),
            (4, 18),
            (7, 18),
        ];
        // 2..=14 are static: the header is in the file and needs no machine state at all.
        for fid in 2..15usize {
            let (rows, wb) = dims[fid];
            let p = i.frame_ptr(fid);
            assert_ne!(p, NO_RECORD, "frame {fid} has a pointer");
            assert_eq!(
                (i.w(p) as usize, i.w(p + 2) as usize),
                (rows, wb),
                "frame {fid} is {rows} rows by {wb} bytes"
            );
            assert!(
                p + 4 + rows * wb <= i.ds.len(),
                "frame {fid} fits in the load image"
            );
        }
        // 0 and 1 are the runtime cut buffers: empty in the file, 28x14 after the entry cuts.
        for fid in 0..2usize {
            let p = i.frame_ptr(fid);
            assert_eq!(p, A_CUTBUF[fid], "frame {fid} is cut into its own buffer");
            assert_eq!(
                (i.w(p), i.w(p + 2)),
                (0, 0),
                "frame {fid} has no header until the entry cuts it"
            );
        }
        i.boot_card();
        i.boot_scene();
        for (fid, xb0, slot) in [(0usize, 19usize, 0usize), (1, 56, 1)] {
            let p = i.frame_ptr(fid);
            let (rows, wb) = (28usize, 14usize);
            assert_eq!(
                (i.w(p) as usize, i.w(p + 2) as usize),
                (rows, wb),
                "the cut gives frame {fid} its shape"
            );
            for r in 0..rows {
                let rec = &i.ds[p + 4 + r * wb..p + 4 + (r + 1) * wb];
                let scene = &title[(166 + r) * 80 + xb0..(166 + r) * 80 + xb0 + wb];
                assert_eq!(
                    rec, scene,
                    "frame {fid} row {r} is TITLE.DAT row {} byte {xb0}",
                    166 + r
                );
            }
            assert_eq!(
                i.w(i.slot_ptr(slot)) as usize,
                rows * wb,
                "slot {slot}'s count is the cut's byte count"
            );
        }
        // record 2 is exactly TITLE.DAT rows 97..129, bytes 22..54 (the measured identity)
        let p = i.frame_ptr(2);
        let (rows, wb) = (33usize, 33usize);
        for r in 0..rows {
            let rec = &i.ds[p + 4 + r * wb..p + 4 + (r + 1) * wb];
            let scene = &title[(97 + r) * 80 + 22..(97 + r) * 80 + 22 + wb];
            assert_eq!(rec, scene, "record 2 row {r} is TITLE.DAT row {}", 97 + r);
        }
        // the two masked pieces have a real 2 bpp alpha plane, not a stencil
        for (mid, fid) in [(0usize, 4usize), (1, 5)] {
            let m = i.mask_ptr(mid);
            assert_ne!(m, NO_RECORD, "mask {mid} has a pointer");
            assert_eq!(
                (i.w(m) as usize, i.w(m + 2) as usize),
                (dims[fid].0, dims[fid].1),
                "mask {mid} has the shape of its piece (rows first, then bytes)"
            );
            let body = &i.ds[m + 4..m + 4 + 34 * 18];
            assert!(
                body.iter().any(|b| *b != 0 && *b != 0xFF),
                "mask {mid} mixes coverage inside a byte (it is alpha, not a stencil)"
            );
        }
    }

    /// **The caption band is a `TEXTn.DAT` crop.** The machine's `DS:0x9CF0` selector produces the
    /// order 1, none, 2, none, 3 and then hands over to script 1 -- and the rows it writes are the
    /// files' own payload, so no caption is ever measured.
    #[test]
    fn the_captions_are_text_dat_crops() {
        let Some(mut i) = intro() else { return };
        let dir = orig::repo().join(orig::ORIGINAL_DIR);
        let texts: Vec<Vec<u8>> = (1..=3)
            .map(|n| fs::read(dir.join(format!("TEXT{n}.DAT"))).unwrap())
            .collect();
        for (n, t) in texts.iter().enumerate() {
            assert_eq!((t[0], t[1]), (54, 80), "TEXT{} is a 54x80 band", n + 1);
        }
        i.boot_card();
        i.boot_scene();
        i.pos = 1;
        let mut seen = Vec::new();
        while i.step() {
            let s = i.strip();
            if seen.last() != Some(&s) {
                seen.push(s);
            }
            if seen.len() > 8 {
                break;
            }
        }
        assert_eq!(
            &seen[..6],
            &[0u8, 1, 0, 2, 0, 3],
            "the caption order is 1, none, 2, none, 3 (spec §4.7)"
        );
        // one caption frame, pixel for pixel: 54 rows of the file's payload over the scene's top
        let mut i2 = intro().unwrap();
        i2.boot_card();
        i2.boot_scene();
        i2.pos = 1;
        while i2.strip() != 1 && i2.step() {}
        assert_eq!(i2.strip(), 1, "found a TEXT1 frame");
        assert!(i2.pos < MAX_DISPATCH, "the caption arrives inside the intro");
        let idx = i2.indices(); // one palette index per pixel, 0..3
        let mut wrong = 0;
        for y in 0..54 {
            for xb in 0..80 {
                let want = texts[0][2 + y * 80 + xb];
                for k in 0..4 {
                    let w = (want >> (6 - 2 * k)) & 3;
                    let got = idx[y * W + xb * 4 + k];
                    if got != w {
                        wrong += 1;
                    }
                }
            }
        }
        assert_eq!(wrong, 0, "the caption band is TEXT1.DAT's payload, {wrong} px differ");
        // the "caption removed" state is the scene's own top: at DS:0x9CF0 = 1 (and 3) the band
        // blit copies rows 0..53 of DS:0xC42, so before the cave creatures start (dispatch ~670)
        // the band is exactly TITLE.DAT rows 0..53.
        let title = fs::read(dir.join("TITLE.DAT")).unwrap();
        let mut i3 = intro().unwrap();
        i3.boot_card();
        i3.boot_scene();
        i3.pos = 1;
        let mut saw1 = false;
        while i3.pos < MAX_DISPATCH {
            saw1 |= i3.strip() == 1;
            if !i3.step() {
                break;
            }
            if saw1 && i3.strip() == 0 {
                break;
            }
        }
        assert_eq!(i3.strip(), 0, "reached the caption-less state");
        let idx = i3.indices();
        let mut band = 0;
        for y in 0..54 {
            for xb in 0..80 {
                let want = title[y * 80 + xb];
                for k in 0..4 {
                    let w = (want >> (6 - 2 * k)) & 3;
                    if idx[y * W + xb * 4 + k] != w {
                        band += 1;
                    }
                }
            }
        }
        assert_eq!(band, 0, "the caption-less band is the scene's top ({band} px differ)");
    }

    /// Determinism and the frame index: the same frame number always gives the same screen, and
    /// seeking backwards is allowed (the window steps forwards, the CLI seeks anywhere).
    #[test]
    fn seeking_is_deterministic_and_ordered() {
        let Some(mut i) = intro() else { return };
        i.seek(40);
        let a = i.video().to_vec();
        i.seek(300);
        let b = i.video().to_vec();
        assert_ne!(a, b, "the intro animates");
        assert_eq!(i.seek(40), 40);
        assert_eq!(i.video(), &a[..], "seeking 40 twice gives the same screen");
        assert_eq!(i.seek(LAST_STATE), LAST_STATE);
        // the intro really is self-terminating at LAST_STATE
        assert_eq!(i.seek(LAST_STATE + 100), LAST_STATE, "the script ends by itself");
        // and the last state is not the card
        assert_eq!(i.palette(), 1);
    }
}
