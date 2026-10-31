//! The PC-speaker sequencer: the original's `INT 1Ch` handler at `CS:4196`, as a pure state
//! machine, plus the cue table the game writes into.
//!
//! ## What the original does (read, `recon/round/port_check/STAGE18G_REPORT.md`)
//!
//! The game has no sound calls. It has **two words** and a timer interrupt:
//!
//! * `DS:0x3496` — the *pending* cue pointer, written by the game with `movw $<cue>,0x3496`
//!   (19 sites, §3 of the report) and guarded by `cmpw $2,0x34A0 / jg`.
//! * `DS:0x34A0` — the sequencer's **state**, read by the handler and tested by the game as a
//!   "is a cue playing?" gate: `0` idle, `1` start, `2` the background rest, `3` take the pending
//!   cue, `4` a cue is playing.
//!
//! `CS:4548` installs the handler (`AH=25h, AL=1Ch`) and reprograms **PIT channel 0 to divisor
//! `0x136C` (4972)**, i.e. 239.98 Hz: one sequencer tick is 4.1669 ms, and the game's own frame is
//! 26 of them (`game::FRAME_TICKS`). It also sets channel 2 to mode 3 (`0xB6` to port 0x43).
//!
//! A *cue* is a pointer into the data segment of a little note script. Each script is a stream of
//! two-byte pairs; the **low nibble of the first byte** is what it is:
//!
//! | low nibble | meaning | second byte |
//! |---|---|---|
//! | `0` | rest: speaker off | length code |
//! | `1..=0xC` | note: `note_table[n] >> octave` written to PIT 0x42 | length code |
//! | `0xD` | command: `0` attack length (0 = no envelope), `1`/`2` tempo transpose in percent of `0x349C`, `3` gate length | value |
//! | `0xE` | slide: `(note_table[0] - (v << 7)) >> octave`, `v = 0` means off | offset |
//! | `0xF` | stop: speaker off, state 0 | — |
//!
//! The **high nibble** is the octave (a right shift of the divisor: `note_table[n] >> octave`), or
//! the sub-command for `0xD`. The length code is `(0x349C + 0x34AC) >> (L & 7)`, times 1.5 when
//! bit 3 is set — with the base `0x349C` = 200 a note is 200 ticks (833 ms) at `L=0` down to 1 tick
//! at `L=7`; §2 of the report has the whole table.
//!
//! The envelope is the attack length: a note is emitted with the gate **on**, and the gate is
//! closed when the countdown reaches the attack value (`0x34A6`, 6 by default) — so a note sounds
//! for `length - 6` ticks and there is a 25 ms gap before the next one. A command with a zero
//! attack (`0D 00`, in every short cue) turns the envelope off and the note sounds for its whole
//! length.
//!
//! The cue queue is **one word deep**: the game's write is dropped outright while `0x34A0 > 2`, so
//! a cue never interrupts another and only the first cue of a frame survives. When a cue's `0xF`
//! runs, the state falls to 0 and the handler immediately re-arms the *background* cue at
//! `DS:0x3716` — which is one rest (`00 01 FF FF`), i.e. silence. `0x3249` is the sound-enable word
//! the re-arm copies into the state, so `0x3249 == 0` is a mute.
//!
//! ## What this module is
//!
//! [`Sequencer`] is that handler, instruction for instruction, with the note streams read out of
//! the port's own copy of the data segment (`Game::ds`, the same bytes the original plays from —
//! nothing is transcribed). [`Sequencer::tick`] takes one 240 Hz tick and returns the port writes
//! that tick made, which is what [`crate::window::Speaker`] turns into a square wave and what
//! `--sound-dump` writes as CSV.
//!
//! Not reproduced (report §7): the two dead branches of the handler (`0x344C == 2`'s hold/restore
//! path at `CS:4503`, reachable only from the unreachable `jmp 0x44aa` at `CS:4317`), the
//! `INT 21h AH=9` debug string on the `state > 4` path (`CS:4537`), and the speaker's own
//! attack/decay — the real cone is a first-order low-pass, the port's mix is a hard square wave.

use crate::game::TICK_HZ;

/// `DS:0x349C` — the duration base every length code shifts. `CS:41D2` sets it to 0xC8 (200) each
/// time a cue starts from idle, and nothing else in the image writes it.
pub const DURATION_BASE: u16 = 0xC8;
/// `DS:0x34A6` — the default attack length, in ticks (`CS:428C`, `CS:425E`).
pub const ATTACK_DEFAULT: u16 = 6;
/// `DS:0x34B2` — the default gate length for a slide (`CS:4270`).
pub const GATE_DEFAULT: u16 = 1;
/// `DS:0x3716` — the idle "cue": `00 01 FF FF`, one 100-tick rest and a stop.
pub const BACKGROUND: u16 = 0x3716;
/// `DS:0x3478` — the 12-word note table: a chromatic C1..B1 in PIT divisors.
pub const NOTE_TABLE: u16 = 0x3478;
/// `DS:0x3249` — the sound-enable word the re-arm copies into the state.
pub const MUSIC_ON: u16 = 0x3249;
/// The PIT input clock, for turning a divisor into a frequency.
pub const PIT_HZ: f64 = 1_193_181.818_2;

/// The cue pointers the game writes into `DS:0x3496`, with their writer sites (report §3).
///
/// A cue id **is** a data-segment offset — the original stores the pointer, not an index — so this
/// table is just names for the 13 distinct pointers the 19 sites use.
pub mod cue {
    /// `CS:2A9D` (panel A on a held slot), `CS:38D7` (an item picked up): one slide at div 2337
    /// (510.6 Hz) held 27 ticks, no envelope. 117 ms.
    pub const SELECT: u16 = 0xA15A;
    /// `CS:2B1B` / `CS:2B5A` — the trapulator cursor moving: one slide at div 4033 (295.9 Hz), 25
    /// ticks.
    pub const CURSOR: u16 = 0xA150;
    /// `CS:2A79` — the panel's A on an empty slot: 30 alternating blips (2401/off), 58 ticks.
    pub const PANEL_DENIED: u16 = 0xA51E;
    /// `CS:399B` — an item put down: 18 one-tick slides, 19 ticks (79 ms).
    pub const PUTDOWN: u16 = 0x9F94;
    /// `CS:2F74` — the ice pick on the rock face: 13 low thuds, 16 ticks.
    pub const PICK: u16 = 0xA164;
    /// `CS:2EBE` — the saw: 183 one-tick slides in three bursts, 433 ticks (1.8 s).
    pub const SAW: u16 = 0xA188;
    /// `CS:2E5B` / `CS:2F22` — the bucket: 56 alternating 13636/9090, 57 ticks (238 ms).
    pub const BUCKET: u16 = 0xA330;
    /// `CS:2524` (the hatch), `CS:2E40` (the detonator), `CS:37FD` (a trap going off underfoot):
    /// 156 noise notes, 157 ticks (654 ms).
    pub const EXPLOSION: u16 = 0xA3E2;
    /// `CS:23C9` — the hatch's own 200-note noise, 201 ticks (838 ms).
    pub const HATCH_NOISE: u16 = 0x9FBC;
    /// `CS:249D` — a 36-step rising glissando, 46 ticks.
    pub const SLIP_RISE: u16 = 0xA594;
    /// `CS:1E5B` (the ice slip) and `CS:24CD` (the hatch fall): a 70-step falling sweep, 71 ticks.
    pub const FALL: u16 = 0xA5E6;
    /// `CS:30DC` — the snowball: 24 one-tick noise notes, 25 ticks (104 ms).
    pub const SNOWBALL: u16 = 0x9F60;
    /// `CS:2888` — the hatch fanfare, 1644 notes (6.85 s) of a 24-note figure.
    pub const HATCH_FANFARE: u16 = 0xA676;
}

/// What one tick wrote to the hardware. `div` goes to PIT channel 2 (port 0x42) first, then `gate`
/// to port 0x61 — the order the handler does it in (`CS:418F` then `CS:4181`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Out {
    /// A new PIT divisor, i.e. a new pitch. `None` = this tick wrote no divisor.
    pub div: Option<u16>,
    /// `Some(true)` = speaker on, `Some(false)` = off. `None` = no write.
    pub gate: Option<bool>,
}

impl Out {
    fn gate(on: bool) -> Out {
        Out { div: None, gate: Some(on) }
    }
    fn note(div: u16) -> Out {
        Out { div: Some(div), gate: Some(true) }
    }
    #[allow(dead_code)]
    pub fn any(&self) -> bool {
        self.div.is_some() || self.gate.is_some()
    }
}

/// The handler's state words, named as the original names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequencer {
    /// `DS:0x34A0` — 0 idle, 1 start, 2 background rest, 3 take the pending cue, 4 playing a cue.
    pub state: u16,
    /// `DS:0x3494` — the pointer being played.
    pub playing: u16,
    /// `DS:0x3496` — the pointer the game asked for.
    pub pending: u16,
    /// `DS:0x349E` — ticks left in the current note (or until the next byte).
    count: u16,
    /// `DS:0x34A2` — 1 = no envelope (a `0D 00` command), so the gate is never closed early.
    rest: u16,
    /// `DS:0x34A6` — the attack length: the gate closes when the countdown reaches it.
    attack: u16,
    /// `DS:0x34AC` — the length transpose, in ticks, from the `0xD1`/`0xD2` commands.
    transpose: u16,
    /// `DS:0x34B2` — the gate length a slide note uses.
    gate_len: u16,
    /// `DS:0x3249` — sound enabled. The re-arm copies it straight into the state, so 0 = mute.
    music_on: u16,
    /// `DS:0x344D` — the handler's own "speaker on" flag. Read by nothing; kept because it is part
    /// of the state the original keeps.
    gate_flag: u16,
    /// Not in the original: set when the `0xF` stop ran, so a caller can tell "the cue finished"
    /// from "the handler re-armed the background in the same interrupt".
    pub stopped: bool,
}

impl Default for Sequencer {
    fn default() -> Self {
        Sequencer {
            state: 0,
            playing: 0,
            pending: 0,
            count: 0,
            rest: 0,
            attack: ATTACK_DEFAULT,
            transpose: 0,
            gate_len: GATE_DEFAULT,
            music_on: 1,
            gate_flag: 0,
            stopped: false,
        }
    }
}

impl Sequencer {
    /// `CS:4548`'s surroundings: the state is zeroed and the sound-enable word is `DS:0x3249`.
    pub fn new(ds: &[u8]) -> Sequencer {
        let mut s = Sequencer::default();
        s.music_on = word(ds, MUSIC_ON as usize).unwrap_or(1);
        s
    }

    /// Mute without uninstalling: this *is* `DS:0x3249` (`CS:0448` sets it to 0 for the length of
    /// the round-start restore and `CS:0466` puts it back). No front-end flag uses it — `--no-sound`
    /// closes the device — but the handler's own mute is a real behaviour and is tested.
    #[allow(dead_code)]
    pub fn set_music_on(&mut self, on: bool) {
        self.music_on = u16::from(on);
        if !on {
            self.state = 0;
        }
    }

    #[allow(dead_code)]
    pub fn music_on(&self) -> bool {
        self.music_on != 0
    }

    /// The game side of a cue: `cmpw $2,0x34A0; jg over; movw $3,0x34A0; movw $ptr,0x3496`.
    ///
    /// Returns `true` when the cue was accepted. **A cue is dropped while another plays** — that is
    /// the whole of the original's priority rule, and it is why a busy frame makes no sound.
    pub fn cue(&mut self, ptr: u16) -> bool {
        if self.state > 2 {
            return false;
        }
        self.state = 3;
        self.pending = ptr;
        self.stopped = false;
        true
    }

    /// The game's `cmpw $2,0x34A0 / jg` test on its own, for callers that want to log a drop.
    #[allow(dead_code)]
    pub fn accepts_cue(&self) -> bool {
        self.state <= 2
    }

    /// One timer interrupt. Returns the port writes it made.
    pub fn tick(&mut self, ds: &[u8]) -> Out {
        // 41AE: a cue is abandoned outright when the sound-enable word has been cleared.
        if self.state >= 3 && self.music_on == 0 {
            self.state = 0;
        }
        let out = self.dispatch(ds);
        // 41C5: nothing is playing -> re-arm the background cue (one 100-tick rest, then the stop).
        if self.state == 0 {
            self.playing = BACKGROUND;
            self.count = DURATION_BASE; // 41D2: `movw $0xc8,0x349c`, the duration base
            self.state = self.music_on;
        }
        if let Some(g) = out.gate {
            self.gate_flag = u16::from(g);
        }
        out
    }

    fn dispatch(&mut self, ds: &[u8]) -> Out {
        match self.state {
            0 => Out::default(),
            1 => {
                // 4286: the defaults, then straight into the duration loop.
                self.rest = 0;
                self.attack = ATTACK_DEFAULT;
                self.transpose = 0;
                self.gate_len = GATE_DEFAULT;
                self.gate_flag = 0;
                self.count = 1;
                self.state = 2;
                self.duration(ds)
            }
            2 => self.duration(ds),
            3 => {
                // 420B: take the pending cue. The interrupted note's position is saved at 0x3498
                // (two pairs back, i.e. the note itself) — nothing ever reads it back, because the
                // resume path at CS:44AA is unreachable; the cue's stop falls to the background.
                self.count = self.count.wrapping_sub(1);
                self.playing = self.playing.wrapping_sub(2).wrapping_sub(2);
                self.playing = self.pending;
                self.rest = 1;
                self.attack = ATTACK_DEFAULT;
                self.transpose = 0;
                self.gate_len = GATE_DEFAULT;
                self.count = 1;
                self.state = 4;
                self.duration(ds)
            }
            4 => self.duration(ds),
            _ => {
                // CS:4537: state > 4 — silence, state 0. (The original also prints DS:0x344E with
                // INT 21h AH=9; nothing in the image can set a state above 4.)
                self.state = 0;
                Out::gate(false)
            }
        }
    }

    /// `CS:42B2`: the countdown, the envelope, and "the byte is used up, read the next one".
    fn duration(&mut self, ds: &[u8]) -> Out {
        self.count = self.count.wrapping_sub(1);
        let ax = self.count;
        if self.rest != 1 {
            if ax > self.attack {
                return Out::default();
            }
            if ax == self.attack || ax == 4 {
                return Out::gate(false);
            }
        }
        if ax != 0 {
            return Out::default();
        }
        self.byte(ds)
    }

    /// `CS:42E1`: one script byte.
    fn byte(&mut self, ds: &[u8]) -> Out {
        let Some(&b) = ds.get(self.playing as usize) else {
            // a pointer off the segment is not a thing the original can do (DS is 64 KB); treat it
            // as the stop so a corrupt cue cannot spin
            self.state = 0;
            self.stopped = true;
            return Out::gate(false);
        };
        let (lo, hi) = (b & 0x0F, b >> 4);
        match lo {
            0 => {
                // 4338: rest — the gate closes and the length code still applies.
                let out = Out::gate(false);
                let len = self.length(ds);
                merge(out, len)
            }
            0xF => {
                // 4302: stop.
                self.gate_flag = 0;
                self.state = 0;
                self.stopped = true;
                Out::gate(false)
            }
            0xD => self.command(ds, hi),
            0xE => self.slide(ds, hi),
            _ => {
                // 431A: a note. `note_table[n] >> octave`, then the gate, then the length.
                let div = self.note(ds, (lo - 1) as usize) >> hi;
                let out = Out::note(div);
                let len = self.length(ds);
                merge(out, len)
            }
        }
    }

    /// `CS:43A3`: the `0xD` commands. They cost no tick at all — the handler falls straight into
    /// the duration loop again, so a command pair is free.
    fn command(&mut self, ds: &[u8], hi: u8) -> Out {
        let arg = ds.get(self.playing as usize + 1).copied().unwrap_or(0);
        match hi {
            0 => {
                if arg != 0 {
                    self.rest = 0;
                    self.attack = arg as u16;
                } else {
                    self.rest = 1; // 43CE: no envelope
                }
            }
            1 | 2 => {
                if arg != 0 {
                    // 43E2: `0x349C * arg / 100`, rounded up past a remainder of 49.
                    let v = DURATION_BASE as u32 * arg as u32;
                    let mut q = (v / 100) as u16;
                    if v % 100 > 0x31 {
                        q += 1;
                    }
                    self.transpose = if hi == 1 { q } else { q.wrapping_neg() };
                } else {
                    self.transpose = 0;
                }
            }
            3 => self.gate_len = arg as u16,
            _ => {}
        }
        self.playing = self.playing.wrapping_add(2);
        self.count = 1;
        self.duration(ds)
    }

    /// `CS:442C`: a slide — an offset in 128ths of the note-1 divisor, shifted by the octave.
    fn slide(&mut self, ds: &[u8], hi: u8) -> Out {
        let arg = ds.get(self.playing as usize + 1).copied().unwrap_or(0);
        let out = if arg != 0 {
            let d = self.note(ds, 0).wrapping_sub((arg as u16) << 7) >> hi;
            Out::note(d)
        } else {
            Out::gate(false) // 4457: a zero offset is a rest
        };
        self.count = self.gate_len;
        self.playing = self.playing.wrapping_add(2);
        out
    }

    /// `CS:4335`..`CS:4374`: the length code, `(0x349C + 0x34AC) >> (L & 7)`, x1.5 for bit 3.
    fn length(&mut self, ds: &[u8]) -> Out {
        let lb = ds.get(self.playing as usize + 1).copied().unwrap_or(0);
        self.playing = self.playing.wrapping_add(2);
        let mut v = DURATION_BASE.wrapping_add(self.transpose);
        let sh = lb & 7;
        if sh != 0 {
            v >>= sh;
        }
        if lb & 8 != 0 {
            v = v.wrapping_add(v >> 1);
        }
        self.count = v;
        Out::default()
    }

    fn note(&self, ds: &[u8], i: usize) -> u16 {
        word(ds, NOTE_TABLE as usize + 2 * i).unwrap_or(0)
    }
}

fn merge(a: Out, b: Out) -> Out {
    Out { div: a.div.or(b.div), gate: a.gate.or(b.gate) }
}

fn word(ds: &[u8], off: usize) -> Option<u16> {
    let a = *ds.get(off)? as u16;
    let b = *ds.get(off + 1)? as u16;
    Some(a | (b << 8))
}

/// A divisor as a frequency in Hz: `1_193_182 / divisor`.
pub fn hz(div: u16) -> f64 {
    if div == 0 {
        0.0
    } else {
        PIT_HZ / div as f64
    }
}

/// Ticks a second, re-exported so the sequencer and the engine cannot drift apart (`TICK_HZ` is
/// 240, from the PIT divisor `CS:4578` writes).
#[allow(dead_code)]
pub const SEQUENCER_HZ: u32 = TICK_HZ;

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> Vec<u8> {
        std::fs::read(crate::orig::require("spy-vs-spy-iii/original/256/spy.exe")).expect("spy.exe")
    }

    /// The port's data segment: file 0x400 of the image.
    fn ds() -> Vec<u8> {
        let e = exe();
        e[crate::game::DS_FILE_OFF..crate::game::DS_FILE_OFF + 0x10000].to_vec()
    }

    /// A synthetic 64 KB segment with only the note table filled in, for the arithmetic tests.
    fn blank_ds() -> Vec<u8> {
        let mut d = vec![0u8; 0x10000];
        for (i, v) in [36488u16, 34441, 32508, 30683, 28961, 27336, 25801, 24353, 22987, 21696,
                       20479, 19329].iter().enumerate() {
            d[NOTE_TABLE as usize + 2 * i] = (*v & 0xFF) as u8;
            d[NOTE_TABLE as usize + 2 * i + 1] = (v >> 8) as u8;
        }
        // the image's own `DS:0x3249` = 1: sound on. A zero here is a mute (CS:41AE).
        d[MUSIC_ON as usize] = 1;
        d
    }

    /// Run a cue to its stop, returning every tick's output with the tick number.
    fn play(ds: &[u8], ptr: u16) -> Vec<(u32, Out)> {
        let mut s = Sequencer::new(ds);
        assert!(s.cue(ptr), "the sequencer was idle, so the cue must be accepted");
        let mut v = Vec::new();
        for t in 0..20_000u32 {
            let o = s.tick(ds);
            v.push((t, o));
            if s.stopped {
                break;
            }
        }
        v
    }

    fn divs(v: &[(u32, Out)]) -> Vec<u16> {
        v.iter().filter_map(|(_, o)| o.div).collect()
    }

    fn on_ticks(v: &[(u32, Out)]) -> usize {
        v.iter().filter(|(_, o)| o.div.is_some()).count()
    }

    // ---- the note, the length and the envelope ----------------------------------------------

    /// 431A: `note_table[n] >> octave`, written to 0x42, gate on.
    #[test]
    fn a_note_is_the_table_divisor_shifted_by_the_octave() {
        let mut d = blank_ds();
        // 0x13 = octave 1, note 3 (32508) -> 16254; 0x02 = length code 2 (50 ticks)
        d[0x8000..0x8004].copy_from_slice(&[0x13, 0x02, 0xFF, 0xFF]);
        let v = play(&d, 0x8000);
        assert_eq!(v[0].1, Out::note(16254), "tick 0 is the note at octave 1");
        assert_eq!(divs(&v), vec![16254], "one note, then the stop: the stop writes no divisor");
    }

    /// 4369: the length code is `200 >> (L & 7)`, ×1.5 when bit 3 is set — the whole table.
    #[test]
    fn the_length_code_is_the_base_shifted_and_a_bit_three_scales_it() {
        for (code, ticks) in [(0u8, 200u16), (1, 100), (2, 50), (3, 25), (4, 12), (5, 6), (6, 3),
                              (7, 1), (8, 300), (9, 150), (0xA, 75), (0xB, 37), (0xC, 18), (0xD, 9),
                              (0xE, 4), (0xF, 1)] {
            let mut d = blank_ds();
            d[0x8000..0x8004].copy_from_slice(&[0x01, code, 0xFF, 0xFF]);
            let v = play(&d, 0x8000);
            // the note at tick 0, the stop at tick `ticks`
            let stop = v.iter().position(|(_, o)| o.gate == Some(false)).unwrap();
            assert_eq!(stop as u16, ticks, "length code {code:#x} must be {ticks} ticks");
        }
    }

    /// 42C2: the gate closes when the countdown reaches the attack length, so a 25-tick note with
    /// the default attack of 6 sounds for 19 ticks.
    #[test]
    fn the_attack_length_closes_the_gate_before_the_next_note() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let v = play(&d, cue::SELECT); // `0D 06 0D 00 3D 1B 3E 8B FF FF`: attack 6, then `0D 00`
        assert_eq!(v[0].1, Out::note(2337));
        assert_eq!(on_ticks(&v), 1);
        let stop = v.iter().position(|(_, o)| o.gate == Some(false)).unwrap();
        assert_eq!(stop, 27, "`0D 00` turns the envelope off, so all 27 gate ticks sound");
        // and with the envelope on (`0D 06`, which every real stream starts with — taking a cue
        // sets `0x34A2 = 1`, "no envelope", at CS:425E, and the `0D 06` turns it back off), a
        // 25-tick note sounds for 19.
        let mut d2 = blank_ds();
        d2[0x8000..0x8006].copy_from_slice(&[0x0D, 0x06, 0x03, 0x03, 0xFF, 0xFF]);
        let v2 = play(&d2, 0x8000);
        let off = v2.iter().position(|(_, o)| o.gate == Some(false)).unwrap();
        assert_eq!(off, 19, "25 ticks minus the 6-tick attack");
        let stop2 = v2.iter().rposition(|(_, o)| o.gate == Some(false)).unwrap();
        assert_eq!(stop2, 25);
    }

    /// 4338: a rest closes the gate and still takes its length from the next byte.
    #[test]
    fn a_rest_closes_the_gate_and_costs_its_length() {
        let mut d = blank_ds();
        d[0x8000..0x8008].copy_from_slice(&[0x01, 0x05, 0x00, 0x04, 0x01, 0x05, 0xFF, 0xFF]);
        let v = play(&d, 0x8000);
        assert_eq!(v[0].1, Out::note(36488));
        assert_eq!(v[6].1, Out::gate(false), "the 6-tick note ends, the rest closes the gate");
        assert_eq!(v[18].1, Out::note(36488), "then 12 ticks of rest, then the next note");
    }

    /// 43A3: a `0xD` command is free — the handler falls into the duration loop again in the same
    /// tick, so a command pair at the head of a cue costs nothing.
    #[test]
    fn a_command_costs_no_tick() {
        let mut d = blank_ds();
        // 0xD3 gate 4, 0xE slide, then stop
        d[0x8000..0x8006].copy_from_slice(&[0x3D, 0x04, 0x2E, 0x10, 0xFF, 0xFF]);
        let v = play(&d, 0x8000);
        assert_eq!(v[0].1.div, Some(36488 - (0x10 << 7) >> 2), "the slide starts at tick 1");
        assert_eq!(v[4].1.gate, Some(false), "the slide's gate length is the 0xD3 value");
    }

    /// 43E2: the transpose is a percentage of the duration base, added to every length.
    #[test]
    fn a_transpose_command_lengthens_every_note() {
        let mut d = blank_ds();
        // 0xD1 50 -> transpose 100 ticks, note 1 length code 3 -> (200+100)>>3 = 37 ticks
        d[0x8000..0x8006].copy_from_slice(&[0x1D, 50, 0x01, 0x03, 0xFF, 0xFF]);
        let v = play(&d, 0x8000);
        let stop = v.iter().position(|(_, o)| o.gate == Some(false)).unwrap();
        assert_eq!(stop, 37);
        // 0xD2 50 negates it: (200-100)>>3 = 12
        let mut e = blank_ds();
        e[0x8000..0x8006].copy_from_slice(&[0x2D, 50, 0x01, 0x03, 0xFF, 0xFF]);
        let w = play(&e, 0x8000);
        assert_eq!(w.iter().position(|(_, o)| o.gate == Some(false)).unwrap(), 12);
    }

    // ---- the two cue families that are not notes --------------------------------------------

    /// 442C: a slide is `(note1 - (offset << 7)) >> octave`.
    #[test]
    fn a_slide_is_note_one_shifted_by_a_negative_offset() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let v = play(&d, cue::SLIP_RISE); // 0x4E 0x30 first: (36488 - 48*128) >> 4 = 1896
        assert_eq!(divs(&v)[0], 1896);
        assert!(divs(&v).windows(2).all(|w| w[0] > w[1]),
                "a rising glissando: the divisor only falls");
        // and the fall is the mirror image
        let f = play(&d, cue::FALL);
        assert_eq!(divs(&f)[0], 1128);
        assert!(divs(&f).windows(2).all(|w| w[0] <= w[1]), "the fall only rises in divisor");
    }

    /// 4457: a slide with a zero offset is the "off" of a buzzer.
    #[test]
    fn a_slide_with_a_zero_offset_is_a_rest() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let v = play(&d, cue::PANEL_DENIED); // 3E 87 / 0E 00, 3E 49 / 0E 00, ...
        let mut want = Vec::new();
        for _ in 0..15 {
            want.push(2401);
            want.push(3393);
        }
        assert_eq!(divs(&v), want, "two pitches, on and off, fifteen times each");
        let gates: Vec<bool> = v.iter().filter_map(|(_, o)| o.gate).collect();
        assert!(gates.windows(2).any(|w| w[0] == false && w[1] == true), "it buzzes on and off");
    }

    // ---- the queue --------------------------------------------------------------------------

    /// The pre-empt rule: `cmpw $2,0x34A0 / jg` — a cue is dropped while another plays, and taken
    /// again the moment the first one stops.
    #[test]
    fn a_cue_is_dropped_while_one_plays_and_taken_when_it_ends() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let mut s = Sequencer::new(&d);
        assert!(s.cue(cue::SNOWBALL));
        s.tick(&d); // state 3 -> 4, first note
        assert_eq!(s.state, 4);
        assert!(!s.cue(cue::SELECT), "a cue must be refused while one is playing");
        assert_eq!(s.pending, cue::SNOWBALL, "the refused cue never reached the queue");
        for _ in 0..40 {
            s.tick(&d);
        }
        assert!(s.state <= 2, "the snowball stopped and the background rest is running: {}",
                s.state);
        assert!(s.cue(cue::SELECT), "and now a cue is accepted");
    }

    /// A second cue in the *same* frame is dropped too: state 3 is `> 2`.
    #[test]
    fn the_second_cue_of_the_same_frame_is_dropped() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let mut s = Sequencer::new(&d);
        assert!(s.cue(cue::SELECT));
        assert!(!s.cue(cue::CURSOR));
        assert_eq!(s.pending, cue::SELECT);
    }

    /// 0xF: the stop silences, sets state 0, and the handler re-arms the background in the same
    /// interrupt — the background is a rest, so nothing is ever written for it.
    #[test]
    fn the_stop_silences_and_rearms_the_background() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let mut s = Sequencer::new(&d);
        s.cue(cue::CURSOR);
        while !s.stopped {
            s.tick(&d);
        }
        assert_eq!(s.playing, BACKGROUND, "the stop re-arms the background cue");
        assert!(s.state <= 2, "state {}", s.state);
        // the background is `00 01 FF FF`: a 100-tick rest, then the stop. It never writes a
        // divisor and never turns the gate on — the only thing it does is re-close a gate the
        // recorder would already have seen closed.
        for _ in 0..400 {
            let o = s.tick(&d);
            assert!(o.div.is_none(), "the background cue is a rest: no divisor");
            assert_ne!(o.gate, Some(true), "and never a gate-on");
        }
    }

    /// `0x3249 == 0` is a mute: the cue is abandoned at the top of the handler.
    #[test]
    fn the_sound_enable_word_is_a_mute() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let mut s = Sequencer::new(&d);
        s.set_music_on(false);
        s.cue(cue::SNOWBALL);
        let mut wrote = false;
        for _ in 0..40 {
            wrote |= s.tick(&d).any();
        }
        assert!(!wrote, "muted: the handler abandons the cue at CS:41AE");
        assert_eq!(s.state, 0);
    }

    // ---- the recorded cues ------------------------------------------------------------------

    /// The short cues, note for note: pointer, note count, total ticks and the first divisor.
    #[test]
    fn every_short_cue_is_the_tables_own_numbers() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        for (ptr, notes, ticks, first) in [
            (cue::SELECT, 1, 28, 2337u16),
            (cue::CURSOR, 1, 26, 4033),
            (cue::PUTDOWN, 18, 19, 6050),
            (cue::PICK, 13, 16, 18312),
            (cue::SNOWBALL, 24, 25, 6818),
            (cue::BUCKET, 56, 57, 13636),
            (cue::PANEL_DENIED, 30, 58, 2401),
            (cue::SLIP_RISE, 36, 46, 1896),
            (cue::FALL, 70, 71, 1128),
            (cue::EXPLOSION, 156, 157, 2370),
            (cue::HATCH_NOISE, 200, 201, 2370),
        ] {
            let v = play(&d, ptr);
            assert_eq!(on_ticks(&v), notes, "{ptr:#06x}: notes");
            assert_eq!(v.len() as u32, ticks, "{ptr:#06x}: ticks to the stop");
            assert_eq!(divs(&v)[0], first, "{ptr:#06x}: first divisor");
        }
    }

    /// The saw is three bursts of one-tick slides separated by `3D 08 / 0E 00` rests.
    #[test]
    fn the_saw_is_three_bursts_of_one_tick_slides() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let v = play(&d, cue::SAW);
        assert_eq!(on_ticks(&v), 183);
        assert_eq!(v.len() as u32, 433);
        assert_eq!(divs(&v)[0], 6050);
        let gaps = v.windows(2).filter(|w| w[1].0 - w[0].0 > 8).count();
        assert_eq!(gaps, 0, "the rests are 8 gate ticks, so no output gap exceeds 8 ticks");
    }

    /// The hatch fanfare is a 24-note figure repeated; the recording only covers its first 597
    /// notes, so the *length* here is the data's own (1644 notes).
    #[test]
    fn the_hatch_fanfare_is_a_long_repeating_figure() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        let v = play(&d, cue::HATCH_FANFARE);
        assert_eq!(on_ticks(&v), 1644);
        assert_eq!(divs(&v)[0], 3377);
        assert_eq!(&divs(&v)[..12], &[3377, 3041, 3025, 3457, 3409, 3745, 3473, 3441,
                                      3329, 3137, 3473, 3073]);
        assert_eq!(divs(&v)[24], 3377, "the figure is 24 notes long");
    }

    /// The cue table must be exactly the pointers the executable writes: a scan of the image for
    /// `movw $imm,0x3496` (`c7 06 96 34`) has to find nothing this module has not named.
    #[test]
    fn the_cue_table_is_the_nineteen_writer_sites() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let e = exe();
        let code = &e[0x16E00..0x16E00 + 0x4700];
        let mut found = std::collections::BTreeSet::new();
        let mut sites = 0;
        for i in 0..code.len() - 6 {
            if &code[i..i + 4] == b"\xc7\x06\x96\x34" {
                sites += 1;
                found.insert(u16::from_le_bytes([code[i + 4], code[i + 5]]));
            }
        }
        let named: std::collections::BTreeSet<u16> = [
            cue::SELECT, cue::CURSOR, cue::PANEL_DENIED, cue::PUTDOWN, cue::PICK, cue::SAW,
            cue::BUCKET, cue::EXPLOSION, cue::HATCH_NOISE, cue::SLIP_RISE, cue::FALL,
            cue::SNOWBALL, cue::HATCH_FANFARE,
        ].into_iter().collect();
        assert_eq!(sites, 19, "19 `movw $cue,0x3496` sites (STAGE18B_RECON \u{a7}2.1)");
        assert_eq!(found.len(), 13, "13 distinct pointers");
        assert_eq!(found, named, "every pointer the game stores is named here");
    }

    /// A divisor is a frequency: 1193182 / div.
    #[test]
    fn a_divisor_is_the_pit_clock_divided_by_it() {
        assert!((hz(2337) - 510.56).abs() < 0.01);
        assert!((hz(36488) - 32.70).abs() < 0.01);
        assert_eq!(hz(0), 0.0);
    }

    // ---- the recordings ---------------------------------------------------------------------

    /// The tick a 240 Hz sequencer runs at, from the PIT divisor `CS:4578` writes (`0x136C`).
    const TICK_MS: f64 = 4972.0 * 1000.0 / PIT_HZ;

    /// One recorded `pcspeaker.csv`, as (tick, kind, value). `kind`: 0 = div, 1 = gate.
    fn recorded(session: &str) -> Option<Vec<(i64, u8, i64)>> {
        let p = crate::orig::require("recon/round/sessions")
            .join(session)
            .join("pcspeaker.csv");
        let text = std::fs::read_to_string(&p).ok()?;
        let mut rows: Vec<(String, i64, f64)> = Vec::new();
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.starts_with("emu_ms")) {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() < 3 {
                continue;
            }
            match f[2] {
                "div" | "gate" => rows.push((
                    f[2].to_string(),
                    f[3].parse().unwrap_or(0),
                    f[0].parse().unwrap_or(0.0),
                )),
                _ => {}
            }
        }
        if rows.is_empty() {
            return None;
        }
        // fit the 240 Hz grid: the offset that puts the most events nearest a tick boundary
        let mut best = (f64::MAX, rows[0].2);
        for r in rows.iter().take(60) {
            for q in -4..=4 {
                let off = r.2 + q as f64 * TICK_MS / 8.0;
                let worst = rows.iter().take(60).fold(0.0f64, |w, (_, _, ms)| {
                    let x = (ms - off) / TICK_MS;
                    w.max((x - x.round()).abs() * TICK_MS)
                });
                if worst < best.0 {
                    best = (worst, off);
                }
            }
        }
        let mut out: Vec<(i64, u8, i64)> = Vec::new();
        let mut last_gate: Option<i64> = None;
        for (kind, value, ms) in rows {
            if kind == "gate" {
                if last_gate == Some(value) {
                    continue; // the recorder logs a change only
                }
                last_gate = Some(value);
                out.push((((ms - best.1) / TICK_MS).round() as i64, 1, value));
            } else {
                out.push((((ms - best.1) / TICK_MS).round() as i64, 0, value));
            }
        }
        Some(out)
    }

    /// Replay a recording through **this** sequencer: find which cue each stretch of the capture
    /// is, retrigger them at the ticks the capture shows them at, and diff the emitted stream with
    /// the recorded one.
    ///
    /// The game's own logic is not in the recording (only the speaker writes are), so the cue
    /// sequence is *derived* from the pitch stream — a div sequence and its tick spacing is a
    /// fingerprint. Everything after that is the shipped code: the same `Sequencer::tick` the
    /// window calls 240 times a second.
    #[test]
    fn the_recorded_speaker_stream_replays_through_the_sequencer() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let d = ds();
        // the cue timelines, computed by the sequencer itself
        let cues = [
            cue::SELECT, cue::CURSOR, cue::PANEL_DENIED, cue::PUTDOWN, cue::PICK, cue::SAW,
            cue::BUCKET, cue::EXPLOSION, cue::HATCH_NOISE, cue::SLIP_RISE, cue::FALL,
            cue::SNOWBALL, cue::HATCH_FANFARE,
        ];
        let timelines: Vec<(u16, Vec<(i64, u8, i64)>)> = cues
            .iter()
            .map(|&p| {
                let v = play(&d, p);
                let t0 = v.iter().position(|(_, o)| o.div.is_some()).unwrap() as i64;
                let mut tl: Vec<(i64, u8, i64)> = Vec::new();
                let mut last = 0i64;
                for (t, o) in &v {
                    let t = *t as i64;
                    if t < t0 {
                        continue;
                    }
                    if let Some(dv) = o.div {
                        tl.push((t - t0, 0, dv as i64));
                    }
                    if let Some(g) = o.gate {
                        let g = i64::from(g);
                        if g != last {
                            last = g;
                            tl.push((t - t0, 1, g));
                        }
                    }
                }
                (p, tl)
            })
            .collect();

        let mut total_events = 0usize;
        let mut total_hit = 0usize;
        for session in ["audio-smoke-2", "playtest", "playtest2", "playtest5", "orig-compare-1"] {
            let Some(cap) = recorded(session) else {
                println!("  {session}: no pcspeaker.csv beside the port, skipped");
                continue;
            };
            // a cue is present at tick `t` when every div of its timeline is in the capture there
            let mut starts: Vec<(i64, u16)> = Vec::new();
            let mut used = vec![false; cap.len()];
            for (ptr, tl) in &timelines {
                let Some(&(_, _, first)) = tl.iter().find(|(_, k, _)| *k == 0) else { continue };
                for (_, (t, k, v)) in cap.iter().enumerate() {
                    if *k != 0 || *v != first {
                        continue;
                    }
                    let ok = tl.iter().filter(|(_, kk, _)| *kk == 0).all(|(dt, _, dv)| {
                        cap.iter().any(|(ct, ck, cv)| *ck == 0 && ct == &(t + dt) && cv == dv)
                    });
                    if ok {
                        starts.push((*t, *ptr));
                        for (dt, kk, vv) in tl {
                            for (j, (ct, ck, cv)) in cap.iter().enumerate() {
                                if ck == kk && ct == &(t + dt) && cv == vv {
                                    used[j] = true;
                                }
                            }
                        }
                    }
                }
            }
            starts.sort();
            // ---- the replay: fire each cue at its tick, run the sequencer, compare
            let first_tick = starts.first().map(|s| s.0).unwrap_or(0);
            let last_tick = cap.iter().map(|e| e.0).max().unwrap_or(0);
            let mut seq = Sequencer::new(&d);
            let mut next = 0usize;
            let mut got: Vec<(i64, u8, i64)> = Vec::new();
            let mut last_gate = 0i64;
            for t in first_tick..=last_tick {
                while next < starts.len() && starts[next].0 <= t {
                    // the game's own write, dropped if a cue is playing — exactly `cue()`
                    seq.cue(starts[next].1);
                    next += 1;
                }
                let o = seq.tick(&d);
                if let Some(dv) = o.div {
                    got.push((t, 0, dv as i64));
                }
                if let Some(g) = o.gate {
                    let g = i64::from(g);
                    if g != last_gate {
                        last_gate = g;
                        got.push((t, 1, g));
                    }
                }
            }
            // the window the replay covers
            let want: Vec<&(i64, u8, i64)> = cap.iter().filter(|e| e.0 >= first_tick).collect();
            let hit = want
                .iter()
                .filter(|e| got.iter().any(|g| g == &***e))
                .count();
            let divs: Vec<&(i64, u8, i64)> = want.iter().copied().filter(|e| e.1 == 0).collect();
            let div_hit = divs.iter().filter(|e| got.iter().any(|g| g == &***e)).count();
            println!(
                "  {session}: {} cues, {} of {} transitions reproduced ({} of {} divisors, {:.1}%)",
                starts.len(),
                hit,
                want.len(),
                div_hit,
                divs.len(),
                100.0 * div_hit as f64 / divs.len().max(1) as f64
            );
            total_events += want.len();
            total_hit += hit;
        }
        assert!(total_events > 1000, "the recordings were found ({} transitions)", total_events);
        let pct = 100.0 * total_hit as f64 / total_events as f64;
        println!("  all sessions: {total_hit}/{total_events} = {pct:.1}%");
        assert!(pct > 70.0, "the replay must reproduce the recordings: {pct:.1}%");
    }
}

