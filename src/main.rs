//! Antics -- native recreation of Spy vs. Spy III: Arctic Antics.
//!
//! Milestone 1 is the intro sequence (`spytitl.exe`): the MAXX OUT! card, then the arctic scene
//! with its caption strips and the animated pieces.
//!
//! The intro is a **port of the original's own script engine** (`src/intro.rs`, see
//! `recon/round/port_check/STAGE27A_SPEC.md`): six commands steer ten 12-byte descriptors through
//! a masked blitter and a 54-row band blitter, with the scene held in RAM and the pieces read out
//! of the executable's own records. Every pixel comes from the player's files -- `spytitl.exe`,
//! `TITLE.DAT`, `MAXX.PIC`, `TEXT1..3.DAT` -- and nothing is measured from the original running.
//!
//! CLI (the harness contract from `docs/adr/0002-port-cli-contract.md`):
//!   --shot FILE    render the current frame at native resolution to FILE
//!   --state FILE   write state as JSON to FILE
//!   --frame N      which frame of the sequence to render (default 0)
//!   --data-dir DIR the directory holding the player's nine files (see below)
//!   --assets DIR   the same thing, by the name the harness has always used
//!   --install DIR  copy those nine files out of your own copy of the game into the per-user data
//!                  directory, then exit
//!   --where        print which directory this run resolved to, then exit
//!
//! **No arguments plays** (stage 30). `antics`, `antics --play` and a command line made only of
//! window/data options (`--scale`, `--data-dir`, `--no-sound`, `--fps`, `--panel-auto`, ...) open the
//! window and run the original's own flow: the `spytitl.exe` intro, then the setup selector, then
//! rounds, with Return/Space/Escape or a click skipping the intro. Anything that asks for headless
//! output (`--shot`, `--state`, `--frame`, `--scene`, `--script`, `--tick-trace`, `--demo`,
//! `--install`, `--where`, the recorded-dump switches) keeps exactly the behaviour it had, and
//! `--scene setup --play` / `--scene round --play` are unchanged. A window that cannot open is an
//! error (exit 7), never a silent exit 0.
//!
//! **Quiet by default (v0.1.2).** Nothing informational is printed unless `--verbose` / `-v` is on
//! the command line: the intro's timing, the window size, the speaker, the selector's keys, the
//! per-event item/room lines and the panel notes all go through `note!` (see `src/verbose.rs`).
//! Errors, usage errors, `--install`/`--where` and every flag that exists to print something are
//! unconditional, exactly as before.
//!
//! **No console box on Windows (v0.1.3).** `antics.exe` is a console-subsystem binary and the
//! Windows installer's shortcuts run it with no arguments, so Windows gave it a console of its own:
//! a black box beside the game, and a fatal start-up error printed to a console that closed with
//! the process. A launch that owns its console (`GetConsoleProcessList` sees one process) and is not
//! redirected now detaches it just before the window opens, and repeats a fatal start-up error in a
//! `MessageBoxW` — but only for a plain play, and never with `--verbose`. The decision is
//! `gui_launch_plan`, a pure function tested on every platform; `src/winui.rs` carries it out and
//! exists only on Windows. Launches from `cmd`/PowerShell and every headless path are untouched.
//!
//! The player's files are looked for in this order, so a released binary needs no flags at all: an
//! explicit `--data-dir`/`--assets`, then the per-user data directory (`$XDG_DATA_HOME/antics`,
//! `%APPDATA%\antics`, `~/Library/Application Support/antics`) when `--install` has written a set
//! there, then the development path beside this checkout. The port carries no art: every screen is
//! drawn from those nine files at run time (stage 29).
//!
//! Everything is a pure function of (the player's files, frame index): no wall clock, no RNG, no
//! threads.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
// **v0.1.3** — the one switch `fatal` consults: does a fatal start-up failure also go to a message
// box? Set once from `main`, from `gui_launch_plan(...).box_on_fatal`, so nothing else may flip it.
use std::sync::atomic::{AtomicBool, Ordering};

mod chrome;
mod cpu;
mod game;
mod install;
mod intro;
mod items;
#[cfg(test)]
mod orig;
mod round;
mod selector_draw;
mod sound;
mod terrain;
mod verbose;
#[cfg(feature = "window")]
mod window;
// **v0.1.3** — the Windows console/message-box front-end (`FreeConsole`, `MessageBoxW`,
// `GetConsoleProcessList`). Windows only: the module does not exist on any other target, and the
// decision it serves (`gui_launch_plan`) is platform-independent and tested everywhere.
#[cfg(windows)]
mod winui;

const W: usize = 320;
const H: usize = 200;
const PIXELS: usize = W * H;

/// High-intensity CGA palettes, the same RGB tuples the harness uses (`palette.py`).
const PALETTE_0: [[u8; 3]; 4] = [[0x00, 0x00, 0x00], [0x55, 0xFF, 0x55], [0xFF, 0x55, 0x55], [0xFF, 0xFF, 0x55]];
const PALETTE_1: [[u8; 3]; 4] = [[0x00, 0x00, 0x00], [0x55, 0xFF, 0xFF], [0xFF, 0x55, 0xFF], [0xFF, 0xFF, 0xFF]];

/// One 320x200 CGA screen: one byte per pixel holding a palette index (0-3).
struct Screen {
    idx: Vec<u8>,
}

// ---------------------------------------------------------------------------------------------
// Milestone 2: the setup selector (`spy.exe`)
// ---------------------------------------------------------------------------------------------
//
// **Stage 28b/29.** The selector's frame is *drawn* by a port of the original's own draw list
// (`src/selector_draw.rs`, `spy.exe` CS:0x45B0 and the records it reads) from `spy.exe` +
// `TRAP.PIC`; this port carries no art and reads no measured file. The port still runs the
// original's input machine over the same zones, so a key script drives it exactly as the game does.
//
// Option values are the game's own: `island` is an index 0..6 (the game's raw variable at
// DS:0x31BB is 2x this), `launch` 0 = HIDE sign / 1 = SHOW sign (raw 0x31D7 = 0 or 2), `players`
// raw 0x31DB is 1 for a single spy and 0 for both, `difficulty` index 0..4 displayed as 1..5.
// The cursor zones are the game's `DS:0x2EC0` values: 0 island, 2 launch, 4 players, 6
// difficulty, 8 GO!.

/// The selector's screen: the panel (a port of the original's draw list) plus the ring-less frame
/// it starts from.
struct Setup {
    bg: Screen,
    panel: selector_draw::Panel,
}

/// **The selector's screen**, from the player's own `spy.exe` + `TRAP.PIC` alone.
///
/// `dir` is the resolved data directory (`--data-dir`, the installed per-user directory or the
/// development path). Both files are required: the frame is a port of the original's draw list
/// (`src/selector_draw.rs`), and that list is code in `spy.exe` and pixels in `TRAP.PIC`.
fn load_setup(dir: &Path) -> Result<Setup, String> {
    let panel = selector_draw::Panel::load(dir).ok_or_else(|| {
        let mut missing: Vec<&str> = Vec::new();
        for name in [selector_draw::EXE_NAME, selector_draw::TRAP_PIC_NAME] {
            if !dir.join(name).is_file() {
                missing.push(name);
            }
        }
        if missing.is_empty() {
            format!("spy.exe and TRAP.PIC are in {} but unreadable", dir.display())
        } else {
            format!("{} not in {}", missing.join(" and "), dir.display())
        }
    })?;
    let st = selector_draw::State { cursor: None, ..selector_draw::State::default() };
    let idx = panel
        .render(&st)
        .ok_or_else(|| "the selector's own draw code refused the default state".to_string())?;
    Ok(Setup { bg: Screen { idx }, panel })
}

/// Ticks between two selector redraws — `CS:0F0D` waits for `0x33F4 >= 8` ticks of the 240 Hz PIT,
/// i.e. **30 frames a second**, which is also the between-rounds panel's own rate. The selector is a
/// static screen, so `play_app` redraws it at this rate rather than 240 times a second.
const SETUP_REDRAW: u64 = 8;

/// **stage 23c** — `CS:0F1E`: `if ([0x2F7C] >= 0x3E8)` — 1000 panel frames, i.e. **33.3 s** at
/// 30 Hz, after which the panel starts the next round by itself with the site forced to SHOW.
const PANEL_FRAMES_TO_START: u32 = 1000;

/// The panel's own frame rate, as the code's 8-tick wait gives it.
const PANEL_HZ: u32 = game::TICK_HZ / SETUP_REDRAW as u32;

/// **Is the panel's 1000-frame auto-start armed?**
///
/// The original runs it (there is no switch), but the owner's play-test (2026-10-29) wants a finished
/// round to land on the selection screen and **wait** there, so the port leaves it **off** everywhere
/// unless asked: `--panel-auto on` arms it, `--panel-auto off` (the default) disarms it. `--go` (start
/// in the round) has no panel to time out.
fn panel_auto_enabled(flag: Option<bool>, _play: bool, go: bool) -> bool {
    match flag {
        Some(v) => v && !go,
        None => false,
    }
}

/// The selector's own state and key handling.
///
/// The transitions are the original's, read out of `FUN_26c0_0ed1` and then confirmed by living in
/// the same room as them: Up snaps to zone 0 from the top row and to 2 otherwise, Down goes 0 -> 2
/// then always to 8, Left reaches 4 only from another zone, Right reaches 6 only from another
/// zone, and RETURN acts on the zone it is sitting in. Replaying keys here is what makes the
/// keyboard cycle testable without a window.
struct Selector {
    cursor: u8,
    island: usize,     // 0..6, the game's raw 0x31BB is twice this
    launch: usize,     // 0 = HIDE, 1 = SHOW
    players: usize,    // 0 = one spy, 1 = both spies (raw 0x31DB is the inverse)
    difficulty: usize, // 0..4, displayed as 1..5
    started: bool,
    /// `DS:0x2F7C` — **the panel's frame counter**: one a panel frame, cleared on the timeout
    /// (`CS:0F26`) and by the entry sequence; present exactly while the panel is up.
    panel_frames: u32,
    /// **Stage 24a** — `CS:0F38`'s `DS:0x3203`: the panel timed out and the **attract demo** is what
    /// runs next, not a round the player chose. Cleared by [`panel_entry`], like `started`.
    demo: bool,
}

impl Selector {
    fn new() -> Selector {
        Selector {
            cursor: 0,
            island: 0,
            launch: 0,
            players: 0,
            difficulty: 0,
            started: false,
            panel_frames: 0,
            demo: false,
        }
    }

    fn key(&mut self, k: &str) {
        match k {
            "up" => self.cursor = if self.cursor < 3 { 0 } else { 2 },
            "down" => self.cursor = if self.cursor == 0 { 2 } else { 8 },
            "left" => {
                if self.cursor > 0 {
                    self.cursor = 4;
                }
            }
            "right" => {
                if self.cursor > 0 {
                    self.cursor = 6;
                }
            }
            "return" | "enter" | "space" => match self.cursor {
                0 => self.island = (self.island + 1) % 7,
                2 => self.launch ^= 1,
                4 => self.players ^= 1,
                // `CS:1080` — the difficulty step also clears the panel's frame counter, so a
                // change of level restarts the 1000-frame wait. (The only other write to `0x2F7C`
                // is the timeout's own.)
                6 => {
                    self.difficulty = (self.difficulty + 1) % 5;
                    self.panel_frames = 0;
                }
                8 => self.started = true,
                _ => {}
            },
            _ => {}
        }
    }

    /// **One panel frame** — `CS:0F1A`/`0x0F1E`: `incw [0x2F7C]`, and at 1000 frames
    /// `[0x2F7C] = 0; [0x31BB] = 0; [0x31D7] = 2; [0x3203] = 1; [0x31B0] = 0x469` and a jump to the
    /// island loader. Returns `true` on the frame the panel times out.
    ///
    /// **Stage 24a — what it starts is the ATTRACT DEMO, not a round.** `0x3203` is the flag
    /// `CS:0325` tests to replace the whole input scan with `CS:0203` (the recorded `DEMO.DAT`
    /// stream, both spies, no keyboard, no computer), and `CS:0F4A` jumps into `0x1204` — the same
    /// island-load-and-go the RETURN key takes — so the demo is a *round* in every other respect:
    /// it can end on the clock, on the rocket or on both spies out, and `CS:06D8` then takes it back
    /// to this panel (`0x60D` -> `0x440` -> `0xED1`) with the counter at 0, i.e. a fresh 33 s.
    ///
    /// The island/lsite words the timeout writes are written into the *panel's* own state as well,
    /// because that is what the panel would be drawing (`0x31BB`/`0x31D7` are its island and
    /// launch-site slots) — and the caller builds the round from them via [`demo_opts`], which pins
    /// the rest (`0x3203`, `0x31B0`, `0x0FB6`, and the player count the recorded stream overrides).
    fn panel_frame(&mut self) -> bool {
        self.panel_frames += 1;
        if self.panel_frames < PANEL_FRAMES_TO_START {
            return false;
        }
        self.panel_frames = 0;
        self.island = 0;  // `CS:0F2C`: `[0x31BB] = 0`
        self.launch = 1;  // `CS:0F32`: `[0x31D7] = 2` — SHOW
        self.demo = true; // `CS:0F38`: `[0x3203] = 1` — the attract flag
        true
    }

    fn json(&self) -> String {
        format!(
            "{{\"scene\":\"setup\",\"island\":{},\"launch\":\"{}\",\"players\":{},\
            \"difficulty\":{},\"cursor_zone\":{},\"started\":{},\"demo\":{},\"panel_frames\":{}}}\n",
            self.island + 1,
            if self.launch == 0 { "hide" } else { "show" },
            if self.players == 0 { 1 } else { 2 },
            self.difficulty + 1,
            self.cursor,
            self.started,
            self.demo,
            self.panel_frames
        )
    }
}

/// The selector frame for one state: the drawn frame, or the ring-less one if the original's own
/// draw code refuses a state (it does not, for any state the selector can reach).
fn render_setup(s: &Setup, island: usize, launch: usize, players: usize,
                difficulty: usize, cursor: u8) -> Screen {
    let st = selector_draw::State { cursor: Some(cursor), island, launch, players, difficulty };
    match s.panel.render(&st) {
        Some(idx) => Screen { idx },
        None => Screen { idx: s.bg.idx.clone() },
    }
}

fn write_bmp(path: &Path, screen: &Screen, palette: &[[u8; 3]; 4]) -> Result<(), String> {
    let row_bytes = W * 3;
    let image_size = row_bytes * H;
    let file_size = 54 + image_size;
    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(W as i32).to_le_bytes());
    out.extend_from_slice(&(H as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(image_size as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // BMP rows run bottom-up and the channels are B, G, R. Writing R, G, B turns cyan into
    // yellow and every colour check still "looks fine" -- see ADR 0001.
    for y in (0..H).rev() {
        for x in 0..W {
            let c = palette[screen.idx[y * W + x] as usize];
            out.push(c[2]);
            out.push(c[1]);
            out.push(c[0]);
        }
    }
    fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

/// **stage 18f harness** (`--retreat`) — the human walks away from the computer.
///
/// The stage-17 soak drives the second spy with an empty script, i.e. a statue, and every recorded
/// duel ends when the **human** walks away (stage 18a §4: 5 of the 6 stood-off runs end that way).
/// A soak against a statue therefore measures the statue. This feeds the human a two-axis retreat:
/// away along x, then away along y, switching early when the engine says he is blocked (a doorway
/// he is pressing into). Nothing in the engine reads it — it is an `Input`, like a script's.
fn retreat_input(g: &game::Game, tick: usize) -> game::Input {
    let human = if g.cpu_state(0).is_some() { 1 } else { 0 };
    let o = 1 - human;
    let (hx, hy) = (g.spies[human].x(), g.spies[human].y());
    let (ox, oy) = (g.spies[o].x(), g.spies[o].y());
    let mut k = game::Keys::default();
    let per = game::FRAME_TICKS as usize * 6;
    let vertical = g.spies[human].blocked || (tick / per) % 2 == 1;
    if vertical {
        if hy >= oy { k.down = true } else { k.up = true }
    } else if hx >= ox {
        k.right = true
    } else {
        k.left = true
    }
    if human == 0 {
        game::Input { p1: k, p2: game::Keys::default() }
    } else {
        game::Input { p1: game::Keys::default(), p2: k }
    }
}

/// Run the engine headlessly: apply a script (or `--frame` empty ticks), then write the artefacts.
/// Returns the lines to print.
///
/// This is the ADR 0002 contract applied to gameplay: `frame` in the state JSON equals the number
/// of ticks applied, so a script that was silently ignored cannot pass.
fn run_engine(opts: &game::RoundOpts, exe: Vec<u8>, steps: Vec<game::Input>, upto: Option<usize>,
              shot: Option<PathBuf>, state: Option<PathBuf>, shot_every: Option<(usize, PathBuf)>,
              marker: bool, from_ds: Option<PathBuf>, lift_blocks: bool, check_engine: bool,
              strip_out: Option<PathBuf>, tick_trace: Option<PathBuf>, layout: bool,
              dump_objects: bool, state_every: Option<(usize, PathBuf)>,
              levels_out: Option<PathBuf>, retreat: bool, mut sound_dump: SoundDump,
              ds_out: Option<PathBuf>)
    -> Result<Vec<String>, String>
{
    let mut g = game::Game::new(exe, opts)?;
    let mut report = Vec::new();
    // ---- the round-start object layout, as data ------------------------------------------------
    if layout {
        report.push(format!(
            "layout: {} object(s) in island {}'s five blocks{}",
            g.layout.objects.len(),
            g.data.island + 1,
            g.layout
                .hatch
                .map(|h| format!(", hatch band {} row {} entry {}", h.band, h.row, h.entry))
                .unwrap_or_default()
        ));
        for o in &g.layout.objects {
            report.push(format!(
                "  band {:2} row {} entry {:3} x {:3} code {:#04x}{}",
                o.band,
                o.row,
                o.entry,
                o.world_x(),
                o.code,
                if o.code & 0x80 != 0 { "  (mound: hidden item)" } else { "" }
            ));
        }
    }
    // ---- verification path: draw a recorded dump's state through the engine and diff ----------
    if let Some(p) = &from_ds {
        let st = round::RoundState::from_ds_dump(p)?;
        g.seed_from_dump(&st, lift_blocks);
        if check_engine {
            report.push(format!(
                "engine check vs {} (band {} map_x {}, blocks {})",
                p.display(),
                st.players[0].band,
                st.players[0].map_x,
                if lift_blocks { "lifted from the dump" } else { "engine's own (empty)" }
            ));
            let want = &st.ds[round::DS_STRIP..round::DS_STRIP + round::STRIP_LEN];
            for player in 0..2usize {
                let strip = g.engine_strip(player);
                let diff = strip.iter().zip(want).filter(|(a, b)| a != b).count();
                let bbox = round::diff_box(&strip, want);
                report.push(format!(
                    "  player {}: {} of {} strip bytes differ; box={}{}",
                    player + 1,
                    diff,
                    round::STRIP_LEN,
                    match bbox {
                        Some((r0, r1, c0, c1)) =>
                            format!("rows {r0}..{r1} cols {c0}..{c1}"),
                        None => "none".to_string(),
                    },
                    if diff == 0 { "  EXACT" } else { "" }
                ));
                if diff > 0 {
                    let passes: Vec<String> = g.engine_passes(player).iter()
                        .map(|q| format!("cell@{} lvl{} plate{} cx{} dx{} w{}",
                                         q.cell_off, q.level, q.plate, q.cx, q.dx, q.width))
                        .collect();
                    report.push(format!("    engine passes: [{}]", passes.join("; ")));
                    let est = g.round_state();
                    let ep = round::read_pose(&est.ds, player);
                    let (eb, el) = ep.records(&est.ds);
                    report.push(format!(
                        "    engine pose: set={} dir={} frame={} extra={} mirror={} row_shift={} \
raised={} state={} clip={} -> body {eb:#06x} legs {el:#06x}; spy x={} y={} map_x={}",
                        ep.set, ep.dir, ep.frame, ep.extra, ep.mirror, ep.row_shift,
                        ep.raised, ep.state, ep.clip,
                        g.spies[player].x(), g.spies[player].y(), g.spies[player].map_x));
                    let sample: Vec<String> = (0..round::STRIP_LEN)
                        .filter(|&i| strip[i] != want[i])
                        .take(6)
                        .map(|i| format!("[{},{}] {:#04x}!={:#04x}",
                                         i / round::STRIP_PITCH, i % round::STRIP_PITCH,
                                         strip[i], want[i]))
                        .collect();
                    report.push(format!("    first differences: {}", sample.join(" ")));
                }
            }
        }
        if let Some(p) = &strip_out {
            // raw 44x58 strip bytes per console, player 1 then player 2 — for the stage scripts
            let mut buf = Vec::with_capacity(2 * round::STRIP_LEN);
            for player in 0..2usize {
                buf.extend_from_slice(&g.engine_strip(player));
            }
            fs::write(p, &buf).map_err(|e| format!("{}: {e}", p.display()))?;
            report.push(format!("wrote {} (2 x {} strip bytes)", p.display(), round::STRIP_LEN));
        }
    }
    let apply = upto.unwrap_or(steps.len()).min(steps.len());
    let mut written = 0usize;
    // ---- per-tick state series, for the self-test ----------------------------------------------
    // One JSON object per `state_every` ticks (tick 0 included), line-delimited. The self-test's
    // soak and its differential replay read this; without the flag the engine writes nothing new.
    let mut state_series = String::new();
    if state_every.is_some() {
        state_series.push_str(&g.state_json());
    }
    // Stage 16a: one row per **tick** (the engine's 240 Hz timer tick), with the game frame the
    // tick belongs to and whether it completed one. A frame is 26 of these rows, so a consumer that
    // wants frames filters on `frame_done` (or divides `tick` by `game::FRAME_TICKS`).
    let mut trace = String::from(
        "tick,frame,frame_done,p1_screen,p1_x,p1_y,p1_map_x,p1_dir,p1_frame,p1_step,p1_blank,\
p2_screen,p2_x,p2_y,p2_map_x,p2_dir,p2_frame,p2_step,p2_blank,p1_carried,p2_carried,mask,\
kit1,kit2,p1_frozen,p2_frozen,p1_anim,p2_anim,cpu_action,cpu_intents,cpu_slot,cpu_keys,cpu_cell,\
cpu1_action,cpu1_intents,cpu1_slot,cpu1_keys,cpu1_cell\n");
    for i in 0..apply {
        // **stage 18f** — `--retreat` replaces the script with a human who runs away; see
        // [`retreat_input`]. Everything else in this loop is untouched.
        let step = if retreat { retreat_input(&g, i) } else { steps[i] };
        let done = g.tick(step);
        sound_dump.write(g.ticks, g.sound_out);
        if let Some((every, _)) = &state_every {
            if *every > 0 && (i + 1) % every == 0 {
                state_series.push_str(&g.state_json());
            }
        }
        if tick_trace.is_some() {
            let a = &g.spies[0];
            let b = &g.spies[1];
            trace.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:#06x},{:#06x},{:#06x},{},{},{},{},{},{},{},{},{:#04x}",
                i + 1, g.frames, u8::from(done),
                a.screen, a.x(), a.y(), a.map_x, a.dir, a.frame, a.frame_step,
                u8::from(a.blank),
                b.screen, b.x(), b.y(), b.map_x, b.dir, b.frame, b.frame_step, u8::from(b.blank),
                if a.carried == game::CARRY_NONE_UI { -1 } else { a.carried as i32 },
                if b.carried == game::CARRY_NONE_UI { -1 } else { b.carried as i32 },
                g.mask, a.kit, b.kit,
                a.frozen, b.frozen,
                u8::from(a.anim.is_some() || a.pose_raised),
                u8::from(b.anim.is_some() || b.pose_raised),
                g.cpu_state(1).map(|c| c.action).unwrap_or(99),
                g.cpu_state(1).map(|c| c.intent.iter().map(|&v| (b'0' + v.clamp(0, 9) as u8) as char).collect::<String>()).unwrap_or_default(),
                g.cpu_state(1).map(|c| c.target_slot).unwrap_or(0),
                g.cpu_state(1).map(|_| flags(&g.last_cpu_keys[1])).unwrap_or_default(),
                g.last_cpu_cell[1]));
            // **stage 18f** — the same five columns for the *other* console's computer. Before
            // this the trace could only describe a `--cpu2` round, which is why the punch and the
            // briefcase loop of stage 18f were first seen on the black spy only.
            trace.push_str(&format!(
                ",{},{},{},{},{:#04x}\n",
                g.cpu_state(0).map(|c| c.action).unwrap_or(99),
                g.cpu_state(0).map(|c| c.intent.iter().map(|&v| (b'0' + v.clamp(0, 9) as u8) as char).collect::<String>()).unwrap_or_default(),
                g.cpu_state(0).map(|c| c.target_slot).unwrap_or(0),
                g.cpu_state(0).map(|_| flags(&g.last_cpu_keys[0])).unwrap_or_default(),
                g.last_cpu_cell[0]));
        }
        // **stage 24b** — `--demo-ds DIR`: the round's own data segment, 0x2400..0x3420, one file a
        // completed game frame, so a word-by-word diff against a recorded dump names the rule that
        // put the replay out of step. A recon aid, like `--strip-out`: nothing reads it back.
        if let Some(dir) = &ds_out {
            if done {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                let p = dir.join(format!("f{:05}.bin", g.frames));
                std::fs::write(&p, &g.ds_ref()[0x2400..0x3420])
                    .map_err(|e| format!("{}: {e}", p.display()))?;
                written += 1;
            }
        }
        if let Some((every, dir)) = &shot_every {
            if *every > 0 && (i + 1) % every == 0 {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                let p = dir.join(format!("frame_{:05}.bmp", i + 1));
                write_bmp(&p, &g.frame(marker), &round::ROUND_PALETTE)?;
                written += 1;
            }
        }
    }
    // the last frame always gets a shot when --shot-every is on, so a strip ends where the script does
    if let Some((every, dir)) = &shot_every {
        if *every > 0 {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let p = dir.join(format!("frame_{:05}.bmp", apply));
            write_bmp(&p, &g.frame(marker), &round::ROUND_PALETTE)?;
            written += 1;
        }
    }
    if let Some(p) = &tick_trace {
        fs::write(p, &trace).map_err(|e| format!("{}: {e}", p.display()))?;
        report.push(format!("wrote {}", p.display()));
    }
    if let Some((_, p)) = &state_every {
        fs::write(p, &state_series).map_err(|e| format!("{}: {e}", p.display()))?;
        report.push(format!("wrote {} ({} state line(s))", p.display(),
                            state_series.lines().count()));
    }
    if let Some(p) = &levels_out {
        // this round's level table, 10 rows x 12 u16 little-endian: the self-test's terrain decoder
        // reads it instead of guessing the round-start icicle flip from the island record
        let mut buf = Vec::with_capacity(240);
        for row in &g.levels {
            for w in row {
                buf.extend_from_slice(&w.to_le_bytes());
            }
        }
        fs::write(p, &buf).map_err(|e| format!("{}: {e}", p.display()))?;
        report.push(format!("wrote {} (10 x 12 level words)", p.display()));
    }
    if let Some(p) = shot {
        write_bmp(&p, &g.frame(marker), &round::ROUND_PALETTE)?;
        report.push(format!("wrote {}", p.display()));
    }
    if let Some(p) = state {
        fs::write(&p, g.state_json()).map_err(|e| format!("{}: {e}", p.display()))?;
        report.push(format!("wrote {}", p.display()));
    }
    if dump_objects {
        // every non-zero byte of the five blocks, as (band, row, entry, code) — the before/after
        // oracle for the item actions. Row 0 is included and flagged: it holds a live structure
        // at 0x5E00 in the last block and is never an object.
        let mut n = 0;
        for band in [0u16, 2, 4, 6, 8] {
            if let Some(base) = round::block_addr(&g.ds_ref(), band) {
                for row in 0..4usize {
                    for entry in 0..round::OBJ_ROW_LEN {
                        let b = g.ds_ref()[base + row * round::OBJ_ROW_LEN + entry];
                        if b != 0 {
                            report.push(format!(
                                "obj band {} row {} entry {:3} code {:#04x}{}",
                                band, row, entry, b,
                                if row == 0 { "  (row 0: not an object)" } else { "" }));
                            n += 1;
                        }
                    }
                }
            }
        }
        report.push(format!("objects: {n} non-zero byte(s); carried {} / {}; kit {:#06x} / {:#06x}; mask {:#06x}",
            if g.spies[0].carried == game::CARRY_NONE_UI { -1 } else { g.spies[0].carried as i32 },
            if g.spies[1].carried == game::CARRY_NONE_UI { -1 } else { g.spies[1].carried as i32 },
            g.spies[0].kit, g.spies[1].kit, g.mask));
    }
    report.push(format!(
        "engine: island {} players {} ticks {} clock {}s phase {:?} screen {} x {} y {} map_x {} shots {}",
        g.data.island + 1,
        g.players,
        g.ticks,
        g.clock_secs(),
        g.phase,
        g.spies[0].screen,
        g.spies[0].x(),
        g.spies[0].y(),
        g.spies[0].map_x,
        written
    ));
    if opts.cpu[0] || opts.cpu[1] {
        // stage 17b: a round with a computer in it also says how it ended (the soak reads this);
        // every other report line is unchanged so the older stages' tracked output stays byte-exact
        let last = report.len() - 1;
        report[last].push_str(&format!(" end {:?} end_tick {}", g.end, g.end_tick));
    }
    Ok(report)
}

/// `--sound-dump FILE`: the sequencer's port writes as CSV, one row per write, in the shape the
/// recording rig's own `pcspeaker.csv` uses (minus its `host_mono_ns` and `emu_ms`, which are the
/// session's clock — here the tick number *is* the clock, one tick being 4.1669 ms).
///
/// ```text
/// tick,divisor,gate
/// 12,,3          a gate-on at tick 12
/// 12,2337,3      a new divisor and the gate, on the same tick (a note)
/// 39,,0          the gate closes
/// ```
///
/// The `divisor` column is empty on a gate-only write and the `gate` column is empty on a
/// divisor-only write, which is what the handler does: `CS:418F` writes the PIT and
/// `CS:4181`/`CS:4188` write port 0x61, and either can happen alone. The gate value is the byte
/// the handler puts on port 0x61 — **3** for on (`in al,0x61; or 3; out 0x61`) and **0** for off
/// (`and 0xfc`) — so a dump reads exactly like the rig's `pcspeaker.csv`.
struct SoundDump {
    out: Option<std::io::BufWriter<std::fs::File>>,
    /// The last gate value written. A repeat is **not** logged: the handler re-writes port 0x61
    /// every time it reads a rest (`CS:4338`), and the recording rig drops those too, so this keeps
    /// the dump one-to-one with a `pcspeaker.csv`.
    gate: Option<bool>,
}

impl SoundDump {
    fn new(path: Option<&PathBuf>) -> Result<SoundDump, String> {
        match path {
            None => Ok(SoundDump { out: None, gate: None }),
            Some(p) => {
                let f = std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?;
                let mut out = std::io::BufWriter::new(f);
                use std::io::Write;
                out.write_all(b"tick,divisor,gate\n")
                    .map_err(|e| format!("{}: {e}", p.display()))?;
                Ok(SoundDump { out: Some(out), gate: None })
            }
        }
    }

    fn write(&mut self, tick: u64, o: crate::sound::Out) {
        use std::io::Write;
        let Some(f) = self.out.as_mut() else { return };
        if let Some(d) = o.div {
            // the div comes first: it is the write at CS:418F, the gate is the one after it
            let _ = writeln!(f, "{tick},{d},");
        }
        if let Some(g) = o.gate {
            if self.gate != Some(g) {
                self.gate = Some(g);
                let _ = writeln!(f, "{tick},,{}", if g { 3 } else { 0 });
            }
        }
    }
}

/// The windowed front-end. Two screens: the setup selector, and the round it starts.
///
/// The engine is the same one `--script` drives; this loop only supplies input and draws the result.
/// It ticks at 240 Hz and draws when `tick()` says a game frame ran — 26 ticks, 108.34 ms, which is
/// what the original's loop does (`CS:063c` waits for the timer word, then the frame body runs once).
#[cfg(feature = "window")]
/// The round a GO! press starts: the selector's own choices over the command-line defaults.
///
/// **Stage 11.** The launch site was the one zone the GO! path forgot: it took the island and the
/// player count from the selector but left `launch` at the command line's value (false = HIDE), so
/// a round started from the setup screen with the launch site on **SHOW** ran on HIDE — the hatch
/// invisible until `DS:0x2EB8` reached `0x6C0`, i.e. it appeared only once all three parts were in
/// the briefcase. The owner saw exactly that and read it as a "late reveal".
fn opts_for_selector(base: &game::RoundOpts, sel: &Selector) -> game::RoundOpts {
    let mut o = base.clone();
    o.island = sel.island;
    // both spies are in the round; the one-player option is the *computer*, not a missing spy
    o.players = 2;
    o.launch = sel.launch != 0;
    // **stage 15** — the difficulty zone is no longer "displayed only": `DS:0x31BF` is the round's
    // own word (`CS:1066` steps it by 2 at the options screen and nothing copies it at round start),
    // so the selector's value goes straight into the round.
    o.difficulty = sel.difficulty as u16;
    // **stage 17** — the setup screen's one-player option is the original's `DS:0x31DD == 1`
    // (`CS:0348`): spy 2 becomes the computer. Two players leaves the keyboard to both.
    o.cpu[1] = sel.players == 0;
    o
}

/// **The panel's entry sequence** — `FUN_26c0_046F` (`CS:046F..0x0504`), the part a port that re-uses
/// the real selector has to do.
///
/// What the original does there, in order: both tool strips lit to `0x0FFF` and blitted (a
/// **one-frame flash**, undone four instructions later), the clock forced to `1` and drawn so the pod
/// reads `00:00`, then `[0x2F2C] = 0x0C` on both spies — "nothing in hand", i.e. **whatever they were
/// carrying is dropped** — and the blink pair reset to `1`.
///
/// In the port none of that has to be simulated: the panel is `AppScreen::Setup`, drawn from the
/// captured selector art (which already shows both strips lit and the pod at `00:00`), and every round
/// is a fresh [`game::Game`], so the hands are empty and the kit is at its round-start value by
/// construction.
///
/// **The one word the entry does *not* touch is the counter.** `CS:046F..0x0504` clears nothing at
/// `0x2F7C`; the only writes in the whole segment are the timeout's (`CS:0F26`) and the difficulty
/// step's (`CS:1080`). So the counter **carries across panels**, and the spec's "residue" question is
/// settled by the recording rather than left open:
///
/// * `playtest3` ds_00244 reads `0x2F7C = 438` and ds_00245 (15.000 s later) reads **888** — 450
///   frames in 15.000 s, exactly **30.0 a second**;
/// * that same panel's measured dwell was **18.7 s**, and `(1000 - 438) / 30 = 18.73` s — the dwell
///   is *from the value behind it*, not from zero. A round started with RETURN at frame `N` leaves
///   the next panel `1000 - N` frames, which is why the other sessions' entry dumps read 6, 240, 392
///   and 448 (their own earlier panels), not 0.
fn panel_entry(sel: &mut Selector) {
    sel.started = false;
    sel.demo = false;
}

/// **Stage 24a** — the panel's idle timeout in the port's terms: `CS:0F26..0x0F4A`.
///
/// ```text
/// 0f26  word[0x2F7C] = 0          ; the frame counter starts again
/// 0f2c  word[0x31BB] = 0          ; island 1, whatever the panel was showing
/// 0f32  word[0x31D7] = 2          ; launch site SHOW (`0x31D5` at `CS:1204`)
/// 0f38  word[0x3203] = 1          ; THE ATTRACT FLAG
/// 0f3e  word[0x31B0] = 0x469      ; the placement PRNG's state (DX stays `0x770D`, `CS:20CD`)
/// 0f44  word[0x0FB6] = 0          ; DEMO.DAT from its first frame
/// 0f4a  jmp 0x1204                ; the same island-load-and-go path RETURN takes
/// ```
///
/// It does **not** touch `0x31BF` (the difficulty — a demo round keeps whatever the panel shows) and
/// it does not touch `0x31DB` (the player count): `0x1204` copies that into `0x31DD`, which nothing
/// on the attract path reads, so both spies are in the demo whatever was chosen.
///
/// It also skips `0xED1` — the "back to the panel" reset that clears `0x3203` — which is why the
/// flag survives into the round.
fn demo_opts(base: &game::RoundOpts, stream: &[u16]) -> game::RoundOpts {
    let mut o = base.clone();
    o.island = 0;      // `CS:0F2C` — `[0x31BB] = 0`, the first island
    o.launch = true;   // `CS:0F32` — `[0x31D7] = 2` -> `[0x31D5] = SHOW`
    o.players = 2;     // `CS:1204` -> `[0x31DD] = [0x31DB]`; the recording reads 0 (both spies)
    o.cpu = [false, false]; // `CS:0203` never reaches `CS:129C`
    o.obj_state = Some(round::RNG_ATTRACT_SEED); // `CS:0F3E` — `[0x31B0] = 0x469`
    o.obj_clock = false;                         // ... and `DX` is the fixed `0x770D`
    o.demo = Some(stream.to_vec());
    // The difficulty is the panel's, untouched by the timeout (`0x31BF` is not written).
    o
}

/// The recorded demo's key masks — `DEMO.DAT`, next to `spy.exe` in the original's disk directory.
///
/// The file name is upper case on the 1988 distribution; the game opens `demo.dat` (the string is in
/// the image at `DS:0x0FAD`), so both spellings are tried.
fn load_demo_stream(dir: &std::path::Path) -> Result<Vec<u16>, String> {
    let mut last = String::new();
    for name in ["DEMO.DAT", "demo.dat", "Demo.dat"] {
        let p = dir.join(name);
        match std::fs::read(&p) {
            Ok(b) => return Ok(round::demo_masks(&b)),
            Err(e) => last = format!("{}: {e}", p.display()),
        }
    }
    Err(last)
}

fn play_app(mut win: window::Window, setup: &Setup, mut sel: Selector, opts: game::RoundOpts,
            exe: Vec<u8>, mut marker: bool, start_in_round: bool, sound_on: bool,
            panel_auto: bool, demo_stream: Vec<u16>, mut dump: SoundDump)
    -> Result<(), String>
{
    enum AppScreen {
        Setup,
        Round(Box<game::Game>),
    }
    // The options the *last started* round used. Pressing R restarts with these, not with the
    // command-line defaults: a round started from the selector has to keep the island, the player
    // count and the launch site the player picked there.
    let mut started_opts = opts.clone();
    crate::note!("antics: window {}x ({}x{}) — F1 spy marker, F2 save a BMP, R restart, Esc back",
             win.scale, crate::W as u32 * win.scale, crate::H as u32 * win.scale);
    let mut shot_no = 0u32;
    // **stage 18g** — the PC speaker. `--no-sound` (or a machine with no audio device) leaves it
    // closed: the sequencer still runs, so `--sound-dump` and the tests are unaffected.
    let speaker = if sound_on {
        let sp = window::Speaker::open(0.18);
        match &sp {
            Some(s) => crate::note!("antics: PC speaker on ({} Hz, square wave)", s.rate),
            None => crate::note!("antics: no audio device — running silent"),
        }
        sp
    } else {
        crate::note!("antics: sound off (--no-sound)");
        None
    };
    // The engine ticks at 240 Hz (the original's reprogrammed PIT) but draws **once a game frame**
    // (9.23 a second), which is what `Game::tick`'s return value says. The selector is a static
    // screen, so it is redrawn on a key or every `SETUP_REDRAW` ticks rather than 240 times a second —
    // which is also the panel's own 30 Hz (`CS:0F0D`).
    let mut loop_ticks: u64 = 0;
    let mut screen = if start_in_round {
        crate::note!("antics: starting the round directly (--go)");
        AppScreen::Round(Box::new(game::Game::new(exe.clone(), &opts)?))
    } else {
        crate::note!("antics: setup selector — arrows move, RETURN acts on the highlighted zone, \
                  RETURN on GO! starts the round");
        AppScreen::Setup
    };

    loop {
        loop_ticks += 1;
        let frame = win.pump();
        if frame.quit {
            break;
        }
        if frame.is_pressed("f1") {
            marker = !marker;
            crate::note!("antics: spy marker {}", if marker { "on" } else { "off" });
        }
        match &mut screen {
            AppScreen::Setup => {
                if frame.is_pressed("escape") {
                    break;
                }
                for name in &frame.pressed {
                    if name == "r" || name == "f1" || name == "p" {
                        continue;
                    }
                    sel.key(name);
                }
                // **stage 23c / 24a** — `CS:0F1A`/`0x0F1E`: one panel frame every 8 ticks, and on the
                // 1000th (33.3 s) `CS:0F26..0x0F4A` starts the **attract demo** — not a round the
                // player chose: island 1, the site on SHOW, the attract PRNG state and `DEMO.DAT`
                // driving both spies. Off on headless paths (`--panel-auto`, see
                // `panel_auto_enabled`), because the owner wants a finished round to wait here.
                if panel_auto && !sel.started && !sel.demo && loop_ticks % SETUP_REDRAW == 0
                    && sel.panel_frame()
                {
                    crate::note!(
                        "antics: panel timeout — {PANEL_FRAMES_TO_START} frames ({:.1} s) of no \
                         input: the attract demo, island 1, launch site SHOW, {} recorded frames",
                        PANEL_FRAMES_TO_START as f64 / PANEL_HZ as f64,
                        demo_stream.len()
                    );
                }
                if sel.demo {
                    // `CS:0F4A` -> `0x1204`: the same island-load-and-go as RETURN, with `0x3203`
                    // still set (the timeout path skips `0xED1`, which is what clears it).
                    let o = demo_opts(&opts, &demo_stream);
                    started_opts = o.clone();
                    let g = game::Game::new(exe.clone(), &o)?;
                    crate::note!(
                        "antics: demo — island {}, site SHOW, both spies on the recorded stream \
                         ({} frames), clock {} s, placement state {:#06x}",
                        o.island + 1,
                        g.demo_frames(),
                        game::IslandData::from_exe(&exe, o.island)?.clock_secs,
                        round::RNG_ATTRACT_SEED
                    );
                    screen = AppScreen::Round(Box::new(g));
                    continue;
                }
                if sel.started {
                    let o = opts_for_selector(&opts, &sel);
                    crate::note!(
                        "antics: GO! island {} {} player(s), launch site {}, {} clock",
                        o.island + 1,
                        o.players,
                        if o.launch { "SHOW" } else { "HIDE" },
                        game::IslandData::from_exe(&exe, o.island)?.clock_secs
                    );
                    started_opts = o.clone();
                    let mut g = game::Game::new(exe.clone(), &o)?;
                    // **stage 23a**: the press that started the round is Return, which is also
                    // P1's fire key — the original's flag is consumed by the options screen, so
                    // the round must not see it again (`Game::absorb_held_keys`).
                    g.absorb_held_keys(game::Input { p1: frame.p1, p2: frame.p2 });
                    screen = AppScreen::Round(Box::new(g));
                    continue;
                }
                if !frame.pressed.is_empty() || loop_ticks % SETUP_REDRAW == 0 {
                    let s = render_setup(setup, sel.island, sel.launch, sel.players,
                                         sel.difficulty, sel.cursor);
                    win.present(&s)?;
                }
            }
            AppScreen::Round(g) => {
                if frame.is_pressed("escape") {
                    crate::note!("antics: round abandoned (Escape) — back to the selector");
                    if let Some(sp) = &speaker {
                        sp.silence(); // `CS:051F`: the speaker is turned off on the way out
                    }
                    panel_entry(&mut sel);
                    screen = AppScreen::Setup;
                    continue;
                }
                if frame.is_pressed("r") {
                    crate::note!("antics: restarting the round");
                    if let Some(sp) = &speaker {
                        sp.silence();
                    }
                    screen = AppScreen::Round(Box::new(game::Game::new(exe.clone(),
                                                                      &started_opts)?));
                    if let AppScreen::Round(g) = &mut screen {
                        g.absorb_held_keys(game::Input { p1: frame.p1, p2: frame.p2 });
                    }
                    continue;
                }
                let before = [g.spies[0].screen, g.spies[1].screen];
                let items_before: Vec<(u16, u16, u16)> = (0..2)
                    .map(|i| (g.spies[i].carried, g.spies[i].kit, g.spies[i].dyn_sticks))
                    .collect();
                let ran_frame = g.tick(game::Input { p1: frame.p1, p2: frame.p2 });
                // one tick of the sequencer, straight to the device (and the dump, if asked)
                if let Some(sp) = &speaker {
                    sp.set(g.sound_out);
                }
                dump.write(g.ticks, g.sound_out);
                for i in 0..g.players {
                    let now = (g.spies[i].carried, g.spies[i].kit, g.spies[i].dyn_sticks);
                    if now != items_before[i] {
                        crate::note!(
                            "antics: items P{} -- held {} -> {} ({}), kit {:#06x} -> {:#06x}, \
sticks {}, panel {} at slot {}, mask {:#06x}",
                            i + 1,
                            game::describe_carried(items_before[i].0),
                            game::describe_carried(g.spies[i].carried),
                            game::carried_name(g.spies[i].carried),
                            items_before[i].1, g.spies[i].kit,
                            g.spies[i].dyn_sticks,
                            if g.spies[i].trap_open { "open" } else { "closed" },
                            g.spies[i].trap_cursor / 2,
                            g.mask
                        );
                    }
                }
                for i in 0..g.players {
                    if g.spies[i].screen != before[i] {
                        crate::note!(
                            "antics: room change P{} screen {} -> {} (x {} y {})",
                            i + 1, before[i], g.spies[i].screen, g.spies[i].x(), g.spies[i].y()
                        );
                    }
                }
                if g.phase == game::Phase::Over {
                    // **measured** — every one of the original's three exits lands on the options
                    // screen with no winner and no score: the clock at 00:00 (`CS:065e`), the
                    // ascent's end (`CS:0649`) or both spies out of the round (`CS:06c0`).
                    if let Some(sp) = &speaker {
                        sp.silence();
                    }
                    crate::note!("antics: round over ({}) — back to the selector, no winner",
                             match g.end {
                                 Some(game::RoundEnd::Clock) => "the clock reached 00:00",
                                 Some(game::RoundEnd::Launch) => "the rocket went up",
                                 Some(game::RoundEnd::BothOut) => "both spies are out",
                                 None => "unknown",
                             });
                    if g.is_demo() {
                        crate::note!(
                            "antics: demo over after {} frames ({:.1} s of recorded input) — the \
                             panel is up again with its {:.1} s wait restarted",
                            g.demo_frames(),
                            g.demo_frames() as f64 / 9.23,
                            PANEL_FRAMES_TO_START as f64 / PANEL_HZ as f64
                        );
                    }
                    // `CS:060D` -> `CS:0440` -> `CS:046F`: the round's exit lands on the **panel**,
                    // with the same choices still latched and its counter started again.
                    panel_entry(&mut sel);
                    if panel_auto {
                        crate::note!("antics: panel — GO! starts it, or it starts itself in {} s",
                                 PANEL_FRAMES_TO_START / PANEL_HZ);
                    }
                    screen = AppScreen::Setup;
                    continue;
                }
                if !ran_frame {
                    // a tick, not a frame: the original's loop is still waiting on its timer word,
                    // and it draws nothing until the frame runs.
                    win.pace();
                    continue;
                }
                let s = g.frame(marker);
                if frame.is_pressed("f2") {
                    let p = std::path::PathBuf::from(format!("antics-shot-{shot_no:03}.bmp"));
                    shot_no += 1;
                    win.save(&p, &s)?;
                    crate::note!("antics: saved {}", p.display());
                }
                win.present(&s)?;
            }
        }
        win.pace();
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Stage 30: the intro, in the window
// ---------------------------------------------------------------------------------------------

/// How the intro ended, so the caller knows whether to carry on to the selector.
#[cfg(feature = "window")]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum IntroFlow {
    /// the script machine reached its own end (the quit word `DS:0x9CC8`) — the selector follows
    Finished,
    /// Return, Space, Escape or a click: the player skipped it
    Skipped,
    /// the window was closed: quit, and do not open the selector
    Quit,
}

/// **The recorded number of screens in the intro**: `recon/timeline/meta.json`'s 654 entries
/// (index 0 is the MAXX card, 1..653 the arctic scene). The recorder sampled the original's
/// machine-speed-dependent busy loop, so its capture *k* is only approximately dispatch 2k
/// (`STAGE27A_SPEC.md` §5.1) — the port plays the **states**, spread over the number of screens the
/// recording holds, so the intro body covers itself exactly once and still ends on its own quit
/// word at dispatch 1,840 (1,842 states).
#[cfg(any(feature = "window", test))]
const INTRO_FRAMES: usize = 654;

/// **The MAXX card's hold** — the original's one *absolute* wait: `entry` snapshots the wall clock
/// into `DS:0x9C91` (granularity 4 s) and loops `CS:0x1990` until `>= 0x0F` seconds have passed
/// (`STAGE27A_SPEC.md` §6). 15 s of the port's 240 Hz tick, so `--fps` does not shorten it.
#[cfg(any(feature = "window", test))]
const INTRO_CARD_HOLD_TICKS: u64 = 15 * game::TICK_HZ as u64;

/// **One intro dispatch's length**, in thousandths of a tick.
///
/// The intro body is the recording's 654 screens at one original frame each (`frame_ticks` ticks of
/// the 240 Hz timer), spread over the machine's 1,842 states: `26 * 654 / 1842 = 9.23` ticks a
/// dispatch at the default frame period, i.e. 70.8 s for the whole scene. Integer arithmetic, so
/// the total is exact and the sequence cannot drift.
#[cfg(any(feature = "window", test))]
fn intro_dispatch_period(frame_ticks: u32) -> u64 {
    let states = (intro::LAST_STATE + 1) as u64;
    (frame_ticks.max(1) as u64 * INTRO_FRAMES as u64 * 1000) / states
}

/// **The intro, played in the window** — the first screen of a no-argument run.
///
/// The original's own order is the card, then the arctic scene, then hand over to the game. The
/// port reproduces the machine's **states** (`src/intro.rs`) and paces them on the original's frame
/// period (`game::FRAME_TICKS`, 9.2298 frames a second; `--fps` changes it), with the card's own
/// 15 s hold in front — which is why the whole thing is skippable.
///
/// Return and Space arrive as `return` (`window::key_name`), Escape as `escape`, and a mouse button
/// as `frame.click`; all three skip. A closed window stops the process with status 0.
#[cfg(feature = "window")]
fn play_intro(win: &mut window::Window, mut engine: intro::Intro,
              frame_ticks: u32) -> Result<IntroFlow, String> {
    let period = intro_dispatch_period(frame_ticks);
    let hz = game::TICK_HZ as f64 / frame_ticks.max(1) as f64;
    crate::note!(
        "antics: intro — the MAXX card holds {} s (the original's own wait), then {} screens of \
         the arctic scene at {:.2} a second. Return/Space/Esc or a click skips it.",
        INTRO_CARD_HOLD_TICKS / game::TICK_HZ as u64,
        INTRO_FRAMES,
        hz
    );

    // ---- the card: `entry` + `CS:0x0850` (MAXX.PIC), palette 0 -------------------------------
    engine.seek(0);
    win.present_with(&Screen { idx: engine.indices() }, &PALETTE_0)?;
    let mut ticks: u64 = 0;
    while ticks < INTRO_CARD_HOLD_TICKS {
        let frame = win.pump();
        if frame.quit {
            return Ok(IntroFlow::Quit);
        }
        if frame.click || frame.is_pressed("return") || frame.is_pressed("escape") {
            crate::note!("antics: intro skipped");
            return Ok(IntroFlow::Skipped);
        }
        ticks += 1;
        win.pace();
    }

    // ---- the arctic scene: `boot_scene` (state 1), then one dispatch at a time ---------------
    engine.seek(1);
    win.present_with(&Screen { idx: engine.indices() }, &PALETTE_1)?;
    let states = intro::LAST_STATE + 1;
    let mut shown = 1usize;      // state 1 is on the screen
    let mut acc: u64 = 0;        // thousandths of a tick since the last dispatch
    while shown < states {
        let frame = win.pump();
        if frame.quit {
            return Ok(IntroFlow::Quit);
        }
        if frame.click || frame.is_pressed("return") || frame.is_pressed("escape") {
            crate::note!("antics: intro skipped");
            return Ok(IntroFlow::Skipped);
        }
        acc += 1000;
        let mut drew = false;
        while acc >= period && shown < states {
            acc -= period;
            if !engine.step() {
                // the quit word: `cmd 5` stage 10 counted its sub-counter to 20
                shown = states;
                break;
            }
            shown += 1;
            drew = true;
        }
        if drew {
            win.present_with(&Screen { idx: engine.indices() }, &PALETTE_1)?;
        }
        win.pace();
    }
    crate::note!("antics: intro over — the setup selector");
    Ok(IntroFlow::Finished)
}

/// **stage 22c — who gets the clock-seeded placement.**
///
/// The original's `FUN_26c0_20cd` takes `DX` from the DOS clock (not the attract round's fixed
/// `0x770D`) unless `DS:0x3203` is set, so its item layout changes every round. The port keeps that
/// behind an option, because everything else here is a pure function of its command line: headless
/// runs (`--script`, `--shot`, `--state`, `--layout`, the soaks, every test) have to replay exactly,
/// and the recorded attract round has to stay byte-for-byte. So:
///
/// * `--obj-clock`    force it on, on any path (and it wins over a `--seed`, which the caller asked
///   for explicitly);
/// * `--play`         on, unless `--seed N` or `--obj-state` pins the layout;
/// * `--random-start` the same, and it also rolls a clock-derived AI seed;
/// * everything else  **off** — deterministic, exactly as before this stage.
///
/// `--obj-state` wins inside `Game::new` whatever this returns (it is matched first), which is what
/// keeps `--obj-state 0x469` an exact replay of the attract round.
fn obj_clock_enabled(flag: bool, play: bool, seed_given: bool, obj_state: Option<u16>) -> bool {
    flag || (play && !seed_given && obj_state.is_none())
}

/// `antics --install <DIR>`: check the player's copy, then copy it into the data directory.
///
/// Nothing is generated, decoded or converted: the nine files are the engine's whole input, and the
/// engine draws everything from them at run time. So the report is one line per file copied, the
/// directory it went to, and nothing else.
fn run_install(src: &Path, data_flag: Option<&Path>) -> ExitCode {
    let Some(dest) = install::data_root(data_flag) else {
        eprintln!(
            "no per-user data directory ($HOME is not set): pass --data-dir DIR to choose one"
        );
        return ExitCode::from(2);
    };
    println!("checking {} against Spy vs. Spy III: Arctic Antics (256-colour CGA)", src.display());
    match install::install(src, &dest) {
        Ok(rep) => {
            println!("\ninstalled into {}", rep.dir.display());
            println!("copied {} file(s) and wrote {}:", rep.copied.len(), install::MARKER);
            for f in &rep.copied {
                println!(
                    "  {:<12} {:>7} B",
                    f.path.file_name().unwrap_or_default().to_string_lossy(),
                    f.bytes
                );
            }
            println!(
                "\nantics now draws the intro, the selector and the round from these files. \
                 Run `{}` with no arguments.",
                install::command()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(3)
        }
    }
}

/// The message a player gets when a file the engine needs is not there: the "original game data not
/// found" text when the *whole* set is missing, and the plain `io::Error` when only one file is.
fn missing_or(path: &Path, e: &std::io::Error, dirs: &install::Dirs) -> String {
    if path.is_file() {
        format!("{}: {e}  (data: {})", path.display(), dirs.origin)
    } else {
        install::not_found_looking_in(&[&dirs.originals])
    }
}

// ---------------------------------------------------------------------------------------------
// Stage 30: what a command line asks for — the game, or one of the headless modes
// ---------------------------------------------------------------------------------------------
//
// **The v0.1.0 defect.** `main` defaulted `scene` to `"intro"` and the intro branch was headless
// only (it rendered one frame for `--shot`/`--frame` and returned), while the window was wired into
// the `setup` and `round` branches alone. So `antics` and `antics --play` — the two commands the
// README, the release notes and `--install`'s own success message tell the player to run — exited
// **0** without a window and without a word. The default is now the game: no arguments plays.

/// A flag that only affects the window, or *where the nine files come from*: `Some(true)` when it
/// takes a value, `Some(false)` when it stands alone.
///
/// A command line made of nothing but these still means "play" ([`default_play`]). Everything not
/// listed here — `--shot`, `--frame`, `--scene`, `--script`, `--install`, `--where`, the whole
/// recorded-dump harness — asks for a headless run and keeps the behaviour it had before stage 30,
/// byte for byte.
fn window_only_flag(arg: &str) -> Option<bool> {
    match arg {
        // how the window looks, and the one knob that paces it
        "--scale" | "--fps" => Some(true),
        // where the player's files come from
        "--data-dir" | "--assets" => Some(true),
        // asking for the window itself is a play command
        "--play" => Some(false),
        // the diagnostic chatter: it changes nothing about *what* runs, so `antics -v` plays
        "--verbose" | "-v" => Some(false),
        // the windowed round's speaker, the spy marker, the panel's idle timeout
        "--sound" | "--no-sound" | "--no-panel-auto" | "--marker" | "--no-marker" => Some(false),
        "--panel-auto" => Some(true),
        _ => None,
    }
}

/// Flags that ask for **headless output or a different entry point**: their presence means the
/// command line is not a plain "play", even when `--play` is there too. `--island`, `--launch`,
/// `--players`, `--difficulty`, `--cursor`, `--seed`, `--go` and the round's own switches are *not*
/// on this list: they only choose options for a windowed run.
fn headless_output_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--scene"
            | "--shot"
            | "--state"
            | "--frame"
            | "--layout"
            | "--dump-objects"
            | "--script"
            | "--tick-trace"
            | "--trace"
            | "--keys"
            | "--upto"
            | "--demo"
            | "--demo-frames"
            | "--demo-out"
            | "--demo-ds"
            | "--shot-every"
            | "--state-every"
            | "--strip-out"
            | "--levels-out"
            | "--cpu-frames"
            | "--cpu-probe"
            | "--cpu-corpus"
            | "--ds"
            | "--from-ds"
            | "--check"
            | "--check-engine"
            | "--install"
            | "--where"
    )
}

/// **Does this command line mean "play"?** Yes when it is empty, or consists only of
/// [`window_only_flag`]s and their values — i.e. no flag asks for headless output and none picks a
/// scene.
fn default_play(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        match window_only_flag(&args[i]) {
            Some(true) => {
                // the flag and its value; a flag with no value is a parse error, not a play command
                if i + 1 >= args.len() {
                    return false;
                }
                i += 2;
            }
            Some(false) => i += 1,
            None => return false,
        }
    }
    true
}

/// The first argument that says "headless" — the one to name when `--play` cannot be honoured.
fn first_headless_flag(args: &[String]) -> Option<&str> {
    args.iter().map(String::as_str).find(|a| headless_output_flag(a))
}

/// **Did the player ask for the diagnostic chatter?** `-v` or `--verbose`, anywhere on the command
/// line (it takes no value). Off by default: without it the program prints nothing informational,
/// which is what a player double-clicking the binary expects. See [`verbose`].
fn verbosity_requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "-v" || a == "--verbose")
}

/// **Does this run open on the intro?** Only a plain "play" does: `--play` or no arguments, the
/// intro as the scene, and nothing on the command line asking for headless output or another
/// scene. `--scene setup --play` and `--scene round --play` never reach it.
fn plays_intro(play: bool, scene: &str, args: &[String]) -> bool {
    play && scene == "intro" && first_headless_flag(args).is_none()
}

// ---------------------------------------------------------------------------------------------
// v0.1.3 (Windows): no console box beside the game, and a message box for a fatal start-up error
// ---------------------------------------------------------------------------------------------
//
// **The defect.** `antics.exe` is a console-subsystem binary, and the Windows installer gives the
// player Start Menu and desktop shortcuts that run it with no arguments. Windows therefore opens a
// console for it: a black box beside the game while the intro plays, and — because the process ends
// when the game ends — a box that flashes and vanishes. Worse, the one sentence that says *why* a
// start-up failed went to that console, which closes with the process, so a player who never opened
// a terminal saw a flash and nothing else.
//
// **The fix, in three parts.** (1) The fatal errors of the play path go through [`fatal`], which
// still prints to stderr exactly as before and, for a GUI launch only, repeats the news in a
// `MessageBoxW`. (2) The console is detached ([`detach_console`]) immediately before the window
// opens — after the game data has been found, so every "your files are missing" message still has
// its stderr and, if it is a GUI launch, its box. (3) A launch from `cmd`, PowerShell or Windows
// Terminal is not touched at all: the console belongs to the shell, output is as it always was.
//
// **Why the decision is a separate, pure function.** None of this can be compiled or run on the
// machine this was written on (there is no Windows toolchain), and "does a message box appear on a
// start-up failure" is exactly the kind of behaviour that is expensive to get wrong. So the whole
// decision is `gui_launch_plan`, a pure function of three booleans, with unit tests in
// `launch_tests` that run on every platform; `src/winui.rs` only carries it out.

/// **What a run does about its console** — the answer to [`gui_launch_plan`].
///
/// Two independent-looking questions with one answer each, kept apart because they are read in
/// different places in `main` (the detach just before the window opens, the box on any fatal error
/// along the way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Plan {
    /// Call `FreeConsole` immediately before the window opens, so no black box is left beside it.
    detach_before_window: bool,
    /// Repeat a fatal start-up error in a message box, after printing it to stderr.
    box_on_fatal: bool,
}

/// **The v0.1.3 decision, in one pure function** (unit-tested on Linux, where `is_gui` is always
/// false in a real run; see `launch_tests::the_gui_launch_plan`).
///
/// * `is_gui` — [`gui_launch`]: a Windows launch that owns its console and is not redirected.
/// * `verbose` — `--verbose` / `-v`: the player asked for the console output, so the console stays
///   (detaching it would silence the very thing they asked for) and the message box is not needed
///   (the error is already readable on the console that stayed).
/// * `play` — the command line is a plain play (no headless flag, no `--install`/`--where`). A
///   headless run's console is its output channel, and `--install` runs from a shell by definition.
///
/// Off Windows, or from a terminal, or with `--verbose`, both fields are `false`: nothing changes.
fn gui_launch_plan(is_gui: bool, verbose: bool, play: bool) -> Plan {
    let gui_play = is_gui && play && !verbose;
    Plan { detach_before_window: gui_play, box_on_fatal: gui_play }
}

/// **Is this a GUI launch?** — the Windows half of [`gui_launch_plan`]'s first argument: the console
/// is this process's alone (`GetConsoleProcessList` sees exactly one process attached) *and* neither
/// stream has been redirected. A double-click or a Start Menu shortcut is both; `antics > log.txt`
/// and a launch from any shell are neither.
#[cfg(windows)]
fn gui_launch() -> bool {
    winui::launched_from_gui() && !winui::streams_redirected()
}

/// Off Windows there is no console of our own to own, so nothing is ever a GUI launch and every
/// line below this is inert — Linux and macOS behaviour is byte-identical to v0.1.2.
#[cfg(not(windows))]
fn gui_launch() -> bool {
    false
}

/// Drop the console, on Windows. Off Windows: there is nothing to drop, and the plan never asks.
#[cfg(windows)]
fn detach_console() {
    winui::detach_console();
}

#[cfg(not(windows))]
fn detach_console() {}

/// Is a fatal start-up failure also to be shown in a message box? Set once, in `main`, from the
/// plan; `fatal` only reads it. Never true off Windows (`gui_launch` is `false` there).
static FATAL_BOX: AtomicBool = AtomicBool::new(false);

/// Arm or disarm the message box. Called once, in `main`, right after the plan is computed.
fn set_fatal_boxes(on: bool) {
    FATAL_BOX.store(on, Ordering::SeqCst);
}

/// Is the box armed?
fn fatal_boxes() -> bool {
    FATAL_BOX.load(Ordering::SeqCst)
}

/// **A fatal error on the play path.** Prints the message to stderr exactly as v0.1.2 did — the
/// text, the stream and the exit code at the call site are unchanged — and, when this run started
/// from a shortcut or a double-click on Windows, also puts it in a message box, because that run's
/// console is about to be detached or already has been, and its stderr goes nowhere a player can
/// read.
fn fatal(msg: &str) {
    fatal_telling(msg, msg)
}

/// The same, with a second wording for the box: the terminal keeps the precise text it always had,
/// while the box gets plain words for someone who has never opened a terminal (the data-not-found
/// message is the one that needs this — it is the failure a player who just ran the installer is
/// most likely to meet, and it has to say what to do, not which file was expected where).
fn fatal_telling(msg: &str, gui_msg: &str) {
    // `writeln!` and not `eprintln!`: after `detach_console` (Windows) the process has no console,
    // so a write to stderr **fails**, and `eprintln!` panics when its write fails — the panic would
    // replace the intended exit code and, since it happens before the box below, the player would
    // get no message at all. Same text, same stream, same newline; the error is simply ignored when
    // there is nowhere to write.
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{msg}");
    if fatal_boxes() {
        // Only Windows has a message box. Off Windows the switch can never be armed (`gui_launch`
        // is `false`), so this whole block is the platform split and nothing else.
        #[cfg(windows)]
        crate::winui::message_box("antics", gui_msg, true);
        #[cfg(not(windows))]
        let _ = gui_msg;
    }
}

/// **Open the SDL2 window, or say plainly why not.**
///
/// A released binary has no other way in, so a failed window is never a silent success: the cause
/// SDL gives (no video driver, no `DISPLAY`, a missing backend) is printed with the two things that
/// fix it, and the exit status is 7.
#[cfg(feature = "window")]
fn open_window(scale: u32) -> Result<window::Window, ExitCode> {
    match window::Window::open("antics — Spy vs. Spy III: Arctic Antics", scale) {
        Ok(w) => Ok(w),
        Err(e) => {
            // **v0.1.3** — the same two lines on stderr (one `eprintln!` of the joined text is the
            // same bytes as two), and, for a Windows GUI launch, a box that says it in one sentence:
            // DISPLAY and `apt install` mean nothing to someone who started this from a shortcut.
            fatal_telling(
                &format!(
                    "antics: SDL could not open a window: {e}. Is a display available?\n\
                     antics: set DISPLAY (X11) or WAYLAND_DISPLAY (Wayland) and make sure the SDL2 \
                     video backends are installed (Debian/Ubuntu: libsdl2-2.0-0). On a machine with \
                     no display, the headless modes need no window: `antics --scene setup --shot \
                     FILE`."
                ),
                &format!(
                    "antics could not open its window, so the game cannot start.\n\n{e}\n\n\
                     If another full-screen program is running, close it and try again. If this \
                     keeps happening, restart the computer and update your graphics driver."
                ),
            );
            Err(ExitCode::from(7))
        }
    }
}

fn main() -> ExitCode {
    let mut shot: Option<PathBuf> = None;
    let mut state: Option<PathBuf> = None;
    let mut layout = false;
    let mut dump_objects = false;
    let mut obj_state: Option<u16> = None;
    let mut assets_dir: Option<PathBuf> = None;
    let mut frame_no: u32 = 0;
    let mut scene = String::from("intro");
    let mut island: usize = 0;
    let mut launch: usize = 0;
    let mut start_hatch = false;
    let mut carry: Option<u16> = None;
    let mut carry2: Option<u16> = None;
    let mut carry_at: Vec<(usize, u32, u16)> = Vec::new();
    let mut heat: Option<u16> = None;
    let mut heat2: Option<u16> = None;
    let mut parts = false;
    let mut frame_ticks_opt: Option<u32> = None;
    let mut players: usize = 0;
    let mut difficulty: usize = 0;
    // **stage 17** — `--cpu1` / `--cpu2`: let the computer play that spy. In the original only
    // spy 2 can be a computer (a one-player round); `--cpu1` exists to exercise the routine from
    // the white side too, which the `SI`-relative code supports but the game never asks for.
    let mut cpu1 = false;
    let mut cpu2 = false;
    // `--cpu-frames N`: run N game frames headless with the computer at the controls (and no human
    // keys at all). The demos and the soak runs are built on it.
    let mut cpu_frames: Option<u32> = None;
    // --cpu-probe FILE: seed the round from a recorded dump and print the AI's decision for it.
    let mut cpu_probe: Option<PathBuf> = None;
    // --cpu-corpus DIR: every 1-player dump in DIR, one CSV line of the AI's decision each.
    let mut cpu_corpus: Option<PathBuf> = None;
    let mut cursor: u8 = 0;
    let mut go = false;
    let mut keys: Option<String> = None;
    let mut trace_path: Option<PathBuf> = None;
    let mut upto: Option<usize> = None;
    let mut ds_path: Option<PathBuf> = None;
    let mut check = false;
    let mut both = false;
    let mut objects = true;
    let mut spies = true;
    let mut items_ui = true;
    // ---- the playable round (milestone 3) ----
    let mut play = false;
    // ---- stage 18g: the PC speaker ----
    // `--no-sound` closes the audio device (the windowed path only; a headless run never opens one).
    // `--sound-dump FILE` writes the sequencer's (tick, divisor, gate) stream whatever the mode, so
    // a headless script can be compared with a recorded `pcspeaker.csv`.
    let mut sound_on = true;
    let mut sound_dump_path: Option<PathBuf> = None;
    let mut script_path: Option<PathBuf> = None;
    let mut shot_every: Option<(usize, PathBuf)> = None;
    let mut marker = false;
    // **stage 23c** — `--no-storm`: render the consoles without the Big Storm's overlay. A
    // verification switch only; the round's own gate is the clock (`CS:0B57`).
    let mut no_storm = false;
    // **stage 23c** — `--panel-auto on|off`: the between-rounds panel's 1000-frame (33.3 s) timeout.
    // `None` = off everywhere (owner, 2026-10-29); `--panel-auto on` arms it.
    let mut panel_auto_flag: Option<bool> = None;
    // **stage 24a** — `--demo`: run the attract demo headlessly, from its first recorded frame, for
    // `--demo-frames N` game frames (default: the whole stream, 941 frames). It is the same round
    // the panel's timeout builds (`demo_opts`), so the headless run and the idle panel agree.
    let mut demo = false;
    let mut demo_frames: Option<usize> = None;
    let mut demo_out: Option<PathBuf> = None;
    // `--demo-ds DIR` (stage 24b): the hot window per frame, for a diff against a recorded dump
    let mut demo_ds: Option<PathBuf> = None;
    let mut void_solid = false;
    let mut igloo_entry = game::IglooEntry::Walk;
    let mut visitor_eval_mover_only = false;
    let mut spy_block = true;
    let mut visitor_draw = true;
    let mut strip_out: Option<PathBuf> = None;
    let mut tick_trace_path: Option<PathBuf> = None;
    let mut retreat = false;
    let mut state_every: Option<(usize, PathBuf)> = None;
    let mut levels_out: Option<PathBuf> = None;
    let mut start_x2: Option<i32> = None;
    let mut seed: u32 = 0;
    // **stage 22c** — `--obj-clock`: the placement's `DX` is the DOS clock, as `FUN_26c0_20cd` does in
    // normal play (`round::ObjRng::from_clock`). It is **off** for every headless path (tests, probes,
    // scripts, soaks, demos must replay exactly) and on for the interactive round unless
    // `--seed`/`--obj-state` pins the layout — see `obj_clock_enabled`.
    let mut obj_clock_flag = false;
    let mut seed_given = false;
    let mut random_start = false;
    let mut start_screen: Option<u16> = None;
    let mut start_x: Option<i32> = None;
    let mut start_y: Option<i32> = None;
    let mut scale: u32 = 3;
    let mut clock_override: Option<u16> = None;
    // ---- engine verification against a recorded dump ----
    let mut engine_ds: Option<PathBuf> = None;
    let mut lift_blocks = false;
    let mut check_engine = false;

    // ---- stage 26a: the data directory ----
    // `--install DIR` builds it from the player's own copy of the game; `--data-dir DIR` reads it
    // (and, with `--install`, writes it). Without either, a released binary looks in the per-user
    // data directory and falls back to this checkout's development paths.
    let mut install_from: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut where_only = false;

    let argv: Vec<String> = env::args().skip(1).collect();
    let mut args = argv.iter().cloned();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--shot" => shot = args.next().map(PathBuf::from),
            "--state" => state = args.next().map(PathBuf::from),
            "--assets" => assets_dir = args.next().map(PathBuf::from),
            "--install" => match args.next() {
                Some(v) => install_from = Some(PathBuf::from(v)),
                None => {
                    eprintln!(
                        "--install expects the folder holding your copy of the game\n\
                         e.g. {} --install ~/dos/spy-vs-spy-iii",
                        install::command()
                    );
                    return ExitCode::from(2);
                }
            },
            "--where" => where_only = true,
            "--data-dir" => match args.next() {
                Some(v) => data_dir = Some(PathBuf::from(v)),
                None => {
                    eprintln!("--data-dir expects a directory");
                    return ExitCode::from(2);
                }
            },
            "--play" => play = true,
            // **v0.1.2** — the diagnostic chatter. Recognised here so it is never an "unknown
            // argument"; the switch itself is set once, after the loop (`verbosity_requested`).
            "-v" | "--verbose" => {}
            "--sound" => sound_on = true,
            "--no-sound" => sound_on = false,
            "--sound-dump" => sound_dump_path = args.next().map(PathBuf::from),
            "--script" => script_path = args.next().map(PathBuf::from),
            "--marker" => marker = true,
            "--no-storm" => no_storm = true,
            "--panel-auto" | "--no-panel-auto" => {
                let on = a == "--panel-auto";
                let value = match args.next() {
                    Some(v) if v == "on" || v == "off" => v == "on",
                    Some(other) => {
                        eprintln!("{a} expects on|off, got {other:?}");
                        return ExitCode::from(2);
                    }
                    None => on,
                };
                panel_auto_flag = Some(value);
            }
            "--storm" => no_storm = false,
            "--demo" => demo = true,
            "--demo-frames" => {
                demo_frames = match args.next().and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) if n > 0 => Some(n),
                    _ => {
                        eprintln!("--demo-frames expects a positive number of game frames");
                        return ExitCode::from(2);
                    }
                };
            }
            "--demo-out" => demo_out = args.next().map(PathBuf::from),
            "--demo-ds" => demo_ds = args.next().map(PathBuf::from),
            "--no-marker" => marker = false,
            "--void-solid" => void_solid = true,
            "--void-walkable" => void_solid = false,
            "--igloo-entry" => {
                igloo_entry = match args.next().as_deref() {
                    Some("walk") => game::IglooEntry::Walk,
                    Some("key") => game::IglooEntry::Key,
                    Some("off") => game::IglooEntry::Off,
                    other => {
                        eprintln!("--igloo-entry expects walk|key|off, got {other:?}");
                        return ExitCode::from(2);
                    }
                };
            }
            "--strip-out" => strip_out = args.next().map(PathBuf::from),
            "--layout" => layout = true,
            "--dump-objects" => dump_objects = true,
            "--visitor-draw" => visitor_draw = true,
            "--no-visitor-draw" => visitor_draw = false,
            "--visitor-eval-mover-only" => visitor_eval_mover_only = true,
            "--visitor-eval-both" => visitor_eval_mover_only = false,
            // `CS:097c` / `FUN_26c0_35e5`: the two spies' bodies block each other. `off` is the
            // stage-8 behaviour (walk straight through), kept so the defect and the fix can be
            // measured against the same recording.
            "--spy-block" => {
                spy_block = match args.next().as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    other => {
                        eprintln!("--spy-block expects on|off, got {other:?}");
                        return ExitCode::from(2);
                    }
                };
            }
            "--random-start" => {
                // **stage 22c** — now an alias for `--obj-clock` (every round takes a fresh reading,
                // so it differs the way the original's does) plus the clock-derived AI seed this flag
                // used to roll on its own. Before this stage it only rolled the *object state* once
                // per process, so the `r` key and every GO! replayed the same layout.
                random_start = true;
                obj_clock_flag = true;
                seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
                    .unwrap_or(0);
            }
            "--obj-clock" => {
                // `CS:20cd`'s normal-play branch: `DX` = `int 21h ah=2Ch`, sampled per round.
                obj_clock_flag = true;
            }
            "--obj-state" => {
                // **stage 22c** — hex as well as decimal: the flag's own help text says
                // `0x469 = the attract round`, and `parse::<u16>` never accepted that spelling, so
                // the documented command was an error. Decimal (`1129`) is unchanged.
                let parsed = args.next().and_then(|v| {
                    let v = v.trim().to_string();
                    let (digits, radix) = match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                        Some(h) => (h.to_string(), 16),
                        None => (v, 10),
                    };
                    u16::from_str_radix(&digits, radix).ok()
                });
                obj_state = match parsed {
                    Some(v) => Some(v),
                    None => {
                        eprintln!("--obj-state expects a 16-bit PRNG state (0x469 = the attract round)");
                        return ExitCode::from(2);
                    }
                };
            }
            "--seed" => {
                match args.next().and_then(|v| v.parse::<u32>().ok()) {
                    Some(n) => {
                        seed = n;
                        // an explicit seed **pins the layout**: it turns the clock seeding off for
                        // the interactive path too, so a session (or a bug report) can be repeated
                        seed_given = true;
                    }
                    None => {
                        eprintln!("--seed expects a number");
                        return ExitCode::from(2);
                    }
                }
            }
            "--shot-every" => {
                let n = args.next().and_then(|v| v.parse().ok()).unwrap_or(0usize);
                let dir = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
                shot_every = Some((n, dir));
            }
            "--state-every" => {
                let n = args.next().and_then(|v| v.parse().ok()).unwrap_or(0usize);
                let path = args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("-"));
                if n == 0 {
                    eprintln!("--state-every expects a positive tick interval, then a file");
                    return ExitCode::from(2);
                }
                state_every = Some((n, path));
            }
            "--levels-out" => levels_out = args.next().map(PathBuf::from),
            "--clock" => {
                match args.next().and_then(|v| v.parse::<u16>().ok()) {
                    Some(n) if n > 0 => clock_override = Some(n),
                    _ => {
                        eprintln!("--clock expects a positive number of seconds");
                        return ExitCode::from(2);
                    }
                }
            }
            "--start-hatch" => start_hatch = true,
            // **demo aid** — seed both spies' body-heat word (stage 14): the bar's fill and the
            // freeze are otherwise reached only after 138 frames a step
            "--heat2" => {
                let v = match args.next().and_then(|v| v.parse::<u16>().ok()) {
                    Some(v) => v,
                    _ => {
                        eprintln!("{a} expects a cold value 0..63");
                        return ExitCode::from(2);
                    }
                };
                heat2 = Some(v);
            }
            "--parts" => parts = true,
            // Stage 16a: the frame period is the original's 26 ticks of a 240 Hz timer (108.34 ms,
            // 9.23 frames a second). `--fps F` sweeps it: the tick count is rounded to whole ticks
            // (the original's own word is a tick count) and everything frame-driven follows, so
            // `--fps 10` gives back the old 100 ms frame for a side-by-side.
            "--fps" => {
                let f = match args.next().and_then(|v| v.parse::<f64>().ok()) {
                    Some(f) if f > 0.5 && f < 240.0 => f,
                    _ => {
                        eprintln!("{a} expects a frame rate between 0.5 and 240 (9.2298 = the original)");
                        return ExitCode::from(2);
                    }
                };
                let ticks = (game::TICK_HZ as f64 / f).round() as u32;
                if ticks < 1 || ticks > game::TICK_HZ {
                    eprintln!("{a} {f}: {ticks} ticks a frame is out of range");
                    return ExitCode::from(2);
                }
                frame_ticks_opt = Some(ticks);
                println!("antics: frame = {ticks} ticks = {:.3} ms ({f:.4} frames a second; the \
                          original is {} ticks = {:.3} ms)",
                         ticks as f64 * 1000.0 / game::TICK_HZ as f64, game::FRAME_TICKS,
                         game::FRAME_MS);
            }
            "--heat" => {
                let v = match args.next().and_then(|v| v.parse::<u16>().ok()) {
                    Some(v) => v,
                    _ => {
                        eprintln!("{a} expects a cold value 0..63");
                        return ExitCode::from(2);
                    }
                };
                heat = Some(v);
            }
            // **demo aid** — put an item straight into a spy's hand, as a trapulator take would
            "--carry" | "--carry2" => {
                let v = match args.next().and_then(|v| v.parse::<u16>().ok()) {
                    Some(v) if v <= 12 => v,
                    _ => {
                        eprintln!("{a} expects an item code 1..12");
                        return ExitCode::from(2);
                    }
                };
                if a == "--carry" { carry = Some(v) } else { carry2 = Some(v) }
            }
            // **demo aid** — the same, but at a given game **frame**: `--carry-at FRAME:CODE`
            // (stage 16a: it was a tick, which meant the same number landed on a different frame
            // when the frame period changed — 20 was frame 3 at 6 ticks and frame 0 at 26)
            "--carry-at" | "--carry2-at" => {
                let raw = args.next().unwrap_or_default();
                let (tick, code) = raw.split_once(':').unwrap_or(("", ""));
                let (Ok(tick), Ok(code)) = (tick.parse::<u32>(), code.parse::<u16>()) else {
                    eprintln!("{a} expects FRAME:CODE (an item code 1..12)");
                    return ExitCode::from(2);
                };
                if code > 12 {
                    eprintln!("{a} expects an item code 1..12");
                    return ExitCode::from(2);
                }
                carry_at.push((if a == "--carry-at" { 0 } else { 1 }, tick, code));
            }
            "--start-screen" | "--start-x" | "--start-x2" | "--start-y" | "--scale" => {
                let raw = match args.next() {
                    Some(v) => v,
                    None => {
                        eprintln!("{a} needs a value");
                        return ExitCode::from(2);
                    }
                };
                let n: i32 = match raw.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!("{a} expects a number, got {raw:?}");
                        return ExitCode::from(2);
                    }
                };
                match a.as_str() {
                    "--start-screen" => start_screen = Some(n.max(0) as u16),
                    "--start-x" => start_x = Some(n),
                    "--start-x2" => start_x2 = Some(n),
                    "--start-y" => start_y = Some(n),
                    _ => scale = (n.max(1) as u32).min(8),
                }
            }
            "--scene" => {
                if let Some(v) = args.next() {
                    scene = v;
                }
            }
            "--island" | "--launch" | "--players" | "--difficulty" | "--cursor" | "--frame" => {
                let raw = match args.next() {
                    Some(v) => v,
                    None => {
                        eprintln!("{a} needs a value");
                        return ExitCode::from(2);
                    }
                };
                let n: u32 = match raw.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!("{a} expects a number, got {raw:?}");
                        return ExitCode::from(2);
                    }
                };
                match a.as_str() {
                    "--frame" => frame_no = n,
                    "--island" => island = (n as usize).min(6),
                    "--launch" => launch = (n as usize).min(1),
                    "--players" => players = (n as usize).min(1),
                    "--difficulty" => difficulty = (n as usize).min(4),
                    // cursor zones are the game's own 0/2/4/6/8; accept 0-8 and snap to a zone
                    _ => cursor = if n >= 8 { 8 } else { ((n / 2) * 2) as u8 },
                }
            }
            // **stage 18f harness**: the human runs away (see `retreat_input`). A soak of the
            // computer against a statue measures the statue, not the computer.
            "--retreat" => retreat = true,
            "--cpu1" => cpu1 = true,
            "--cpu2" => cpu2 = true,
            "--cpu-frames" => {
                cpu_frames = match args.next().and_then(|v| v.parse::<u32>().ok()) {
                    Some(n) => Some(n),
                    None => {
                        eprintln!("--cpu-frames expects a positive number of game frames");
                        return ExitCode::from(2);
                    }
                };
            }
            "--cpu-probe" => cpu_probe = args.next().map(PathBuf::from),
            "--cpu-corpus" => cpu_corpus = args.next().map(PathBuf::from),
            "--go" => go = true,
            "--ds" => ds_path = args.next().map(PathBuf::from),
            "--check" => check = true,
            "--both" => both = true,
            "--objects" => objects = true,
            "--spies" => spies = true,
            "--no-items-ui" => items_ui = false,
            // Engine verification: seed the engine from a recorded dump and diff its own
            // composition against the strip the game left in that dump.
            "--from-ds" => engine_ds = args.next().map(PathBuf::from),
            "--lift-blocks" => lift_blocks = true,
            "--check-engine" => check_engine = true,
            "--tick-trace" => tick_trace_path = args.next().map(PathBuf::from),
            "--keys" => keys = args.next(),
            "--trace" => trace_path = args.next().map(PathBuf::from),
            "--upto" => {
                upto = args.next().and_then(|v| v.parse().ok());
            }
            other => {
                eprintln!("unknown argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }

    // **v0.1.2** — the one place the chatter is switched on. Everything informational from here on
    // (the intro's timing, the window, the selector, the round's item/room lines) is `note!`, which
    // prints only when this is true; errors and explicitly-requested output never consult it.
    verbose::set(verbosity_requested(&argv));

    // **stage 30** — `antics` with no arguments (or only window/data options) plays. v0.1.0 fell
    // through to the headless intro here and exited 0, which is exactly what the README, the release
    // notes and `--install`'s success message tell the player *not* to expect.
    let default_play_path = default_play(&argv);
    if !play && default_play_path {
        play = true;
    }
    // **v0.1.3 (Windows)** — the one place the console decision is taken (see `gui_launch_plan`).
    // `gui_launch()` is false off Windows and false when the console is shared with a shell, so
    // both fields are false on every Linux/macOS run and on `antics` typed into a terminal.
    let plan = gui_launch_plan(gui_launch(), verbose::on(), default_play_path);
    set_fatal_boxes(plan.box_on_fatal);
    // The intro is the first screen of a plain "play". A command line that also asks for headless
    // output gets the headless render it asked for — and is told that `--play` was not used, rather
    // than having it silently ignored.
    let intro_play = plays_intro(play, &scene, &argv);
    if play && scene == "intro" && !intro_play {
        if let Some(f) = first_headless_flag(&argv) {
            eprintln!(
                "antics: {f} asks for headless output, so --play is not used here. Run `antics` \
                 with no arguments (or only --scale/--data-dir/--no-sound/--sound/--fps) to play."
            );
        }
    }

    // **stage 23c** — the panel's auto-start policy, resolved once (see `panel_auto_enabled`):
    // on for `--play`, off on every headless path, `--panel-auto on|off` either way.
    let panel_auto = panel_auto_enabled(panel_auto_flag, play, go);

    #[cfg(not(feature = "window"))]
    let _ = scale; // the windowed front-end is compiled out in this build

    let obj_clock = obj_clock_enabled(obj_clock_flag, play, seed_given, obj_state);

    // **stage 26a** -- `--install` first: it does not need a data directory to exist, only a place
    // to put one.
    if let Some(src) = install_from {
        return run_install(&src, data_dir.as_deref());
    }

    // The run-time lookup order, so a released binary works with no flags at all:
    //   1. an explicit `--assets` / `--data-dir`
    //   2. the per-user data directory, when `--install` has written a set there
    //   3. the development path (the unpacked original beside this checkout) -- looked at, never named
    // Every script, demo and test below keeps working because 3 is still the fallback.
    let dirs = match install::resolve(assets_dir.as_deref(), data_dir.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            // **v0.1.3** — the failure a shortcut launch meets most often: the installer ran but no
            // game folder was chosen. Terminal text unchanged; a Windows GUI launch gets the same
            // news in plain words (see `install::not_found_gui`).
            fatal_telling(&e, &install::not_found_gui(&[]));
            return ExitCode::from(4);
        }
    };
    round::RoundAssets::set_original_dir(&dirs.originals);
    if where_only {
        println!("originals: {}", dirs.originals.display());
        println!("from:      {}", dirs.origin);
        return ExitCode::SUCCESS;
    }

    if scene == "round" {
        let dir = dirs.originals.clone();
        // Three modes share this scene:
        //   --play                      interactive window, starting in the round
        //   --script FILE (no --ds)      headless engine, driven by the script
        //   --ds FILE                    the stage-1 dump view (unchanged; the harness uses it)
        let wants_engine = play || script_path.is_some() || ds_path.is_none() || engine_ds.is_some();
        if wants_engine {
            let exe = match fs::read(dir.join("spy.exe")) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("{}", missing_or(&dir.join("spy.exe"), &e, &dirs));
                    return ExitCode::from(3);
                }
            };
            let opts = game::RoundOpts {
                difficulty: difficulty as u16,
                cpu: [cpu1, cpu2],
                island: island.min(6),
                // A computer spy needs both spies in the round (`--cpu2 --players 0` is the
                // original's one-player round: one human, one computer). Without a `--cpu` flag
                // `--players 0` keeps its stage-4 meaning, a solo spy in a round of one.
                players: if cpu1 || cpu2 { 2 } else { (players + 1).clamp(1, 2) },
                seed,
                obj_state,
                // stage 22c: `CS:20cd`'s clock-seeded branch, for the interactive round only
                obj_clock,
                obj_clock_dx: None,
                random_start,
                start_screen,
                start_x,
                start_x2,
                start_y,
                clock_override,
                heat2,
                parts,
                void_solid,
                igloo_entry,
                no_storm,
                visitor_draw,
                visitor_eval_mover_only,
                spy_block,
                objects,
                spies,
                items_ui,
                launch: launch != 0,
                start_hatch,
                carry,
                carry2,
                carry_at: carry_at.clone(),
                heat,
                frame_ticks: frame_ticks_opt,
                demo: None,
            };
            // a script, or `--frame N` as N empty ticks (the ADR's "--frame = which state")
            let mut steps = match &script_path {
                Some(p) => match game::load_script(p) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                },
                None => vec![game::Input::default(); frame_no as usize],
            };
            if let Some(n) = cpu_frames {
                // one game frame is `frame_ticks` ticks (`--fps` can change it); the AI runs on the
                // frame, so N frames is N * frame_ticks empty ticks.
                steps = vec![game::Input::default(); n as usize * opts.frame_ticks.unwrap_or(game::FRAME_TICKS) as usize];
            }

            // ---- stage 24a: the attract demo, headless -------------------------------------------
            //
            // `--demo` builds the *same* round the panel's timeout builds (`demo_opts`): island 1,
            // the site on SHOW, `DS:0x31B0 = 0x469` with the attract `DX = 0x770D`, and `DEMO.DAT`'s
            // 941 key masks driving both spies. The script (if any) only supplies the run length —
            // the frame's keys come from the stream, exactly as `CS:0203` replaces the input scan.
            let mut opts = opts;
            if demo {
                let stream = match load_demo_stream(&dir) {
                    Ok(v) if !v.is_empty() => v,
                    Ok(_) => {
                        eprintln!("{}: no frames in the demo stream", dir.join("DEMO.DAT").display());
                        return ExitCode::from(4);
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(4);
                    }
                };
                let frames = demo_frames.unwrap_or(stream.len());
                let demo_clock = game::IslandData::from_exe(&exe, 0)
                    .map(|d| d.clock_secs)
                    .unwrap_or(0);
                println!(
                    "antics: demo — island 1, site SHOW, {} recorded frames ({} s of input at 9.23 \
                     frames a second), clock {} s, placement state {:#06x} / DX 0x{:02x}{:02x}",
                    stream.len(),
                    stream.len() as f64 / 9.23,
                    demo_clock,
                    round::RNG_ATTRACT_SEED,
                    round::RNG_FIXED_DH,
                    round::RNG_FIXED_DL
                );
                opts = demo_opts(&opts, &stream);
                if demo_out.is_some() {
                    state_every = demo_out
                        .clone()
                        .map(|p| (opts.frame_ticks.unwrap_or(game::FRAME_TICKS) as usize, p));
                }
                steps = vec![
                    game::Input::default();
                    frames * opts.frame_ticks.unwrap_or(game::FRAME_TICKS) as usize
                ];
            }

            if let Some(dirp) = cpu_corpus {
                let exe = match fs::read(dir.join("spy.exe")) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("{}", missing_or(&dir.join("spy.exe"), &e, &dirs));
                        return ExitCode::from(3);
                    }
                };
                let mut names: Vec<PathBuf> = match fs::read_dir(&dirp) {
                    Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path())
                        .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("ds_")).unwrap_or(false))
                        .collect(),
                    Err(e) => {
                        eprintln!("{}: {e}", dirp.display());
                        return ExitCode::from(4);
                    }
                };
                names.sort();
                for path in names {
                    let Ok(ds) = fs::read(&path) else { continue };
                    // One-player **and** not the attract demo: `DS:0x3203` non-zero makes
                    // `CS:0325` replay the recorded demo inputs (`CS:0203`) and never call the AI,
                    // so a demo dump is not an AI frame at all (m1-fix-960's 221 are all demos).
                    if ds.len() != 0x10000
                        || word_at(&ds, 0x31dd) != 1
                        || word_at(&ds, 0x3203) != 0
                    {
                        continue;
                    }
                    for i in [0usize, 1] {
                        if i == 0 && !cpu1 {
                            continue;
                        }
                        if i == 1 && !cpu2 {
                            continue;
                        }
                        let w = cpu::World::from_dump(&ds, i, &exe);
                        let mut c = cpu::Cpu::from_dump(&ds, i);
                        let mut rng = game::Rng::new(0xC0FFEE);
                        let due = c.intent.iter().all(|&v| v == 0) || c.countdown - 1 < 1;
                        // "fresh": the recorded state is the one `CS:1ca9` leaves behind — exactly
                        // one intent set to 1 and the countdown reloaded to 10. That is the only
                        // state in which the recorded `0x2F90` is certainly this decision's output.
                        let fresh = c.intent.iter().filter(|&&v| v == 1).count() == 1
                            && c.countdown == 10;
                        let (k, _s) = c.think(&w, &mut rng);
                        let rec = recorded_keys(&ds, i);
                        println!(
                            "{},{},{},{},{},{},{},{},{},{},{},{}",
                            dirp.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                            path.file_name().unwrap().to_string_lossy(),
                            i + 1,
                            flags(&k),
                            c.action,
                            if due { 1 } else { 0 },
                            word_at(&ds, 0x2f90),
                            rec,
                            word_at(&ds, 0x31bf) / 2,
                            word_at(&ds, 0x324d),
                            word_at(&ds, 0x2fb0),
                            if fresh { 1 } else { 0 },
                        );
                    }
                }
                return ExitCode::SUCCESS;
            }

            if let Some(dp) = cpu_probe {
                let ds = match fs::read(&dp) {
                    Ok(b) if b.len() == 0x10000 => b,
                    Ok(b) => {
                        eprintln!("{}: {} bytes, not a 64 KB data-segment dump", dp.display(), b.len());
                        return ExitCode::from(4);
                    }
                    Err(e) => {
                        eprintln!("{}: {e}", dp.display());
                        return ExitCode::from(4);
                    }
                };
                let exe = match fs::read(dir.join("spy.exe")) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("{}", missing_or(&dir.join("spy.exe"), &e, &dirs));
                        return ExitCode::from(3);
                    }
                };
                let want: Vec<usize> = if cpu1 || cpu2 {
                    [0usize, 1].into_iter().filter(|&i| if i == 0 { cpu1 } else { cpu2 }).collect()
                } else {
                    vec![1]
                };
                for i in want {
                    let w = cpu::World::from_dump(&ds, i, &exe);
                    let mut c = cpu::Cpu::from_dump(&ds, i);
                    if std::env::var("ANTICS_CPU_VIEW").is_ok() {
                        eprintln!(
                            "view {}: anim {} frozen {} ground {} cell {:#04x} carried {} x {} y {} screen {} col {} row {} visitors {:?} sink {:?} mask {:#06x} clock {} fires {} kit {:#06x} heat {} trap {} cursor {} latch {} blocked {} links {:?}",
                            i + 1, w.anim[i], w.frozen[i], w.ground[i], w.cell[i], w.carried[i],
                            w.x[i], w.y[i], w.screen[i], w.cell_col[i], w.cell_row[i],
                            w.visitors, w.sink, w.mask, w.clock_left, w.fires, w.kit[i],
                            w.heat[i], w.trap_open[i], w.trap_cursor[i], w.act_latch[i],
                            w.blocked[i], w.links
                        );
                    }
                    let mut rng = game::Rng::new(0xC0FFEE);
                    let (k, _side) = c.think(&w, &mut rng);
                    println!(
                        "cpu {} keys {}{}{}{}{} action {} intents {:?} countdown {} slot {} found {:#04x}",
                        i + 1,
                        if k.up { "U" } else { "-" },
                        if k.down { "D" } else { "-" },
                        if k.left { "L" } else { "-" },
                        if k.right { "R" } else { "-" },
                        if k.fire { "F" } else { "-" },
                        c.action,
                        c.intent,
                        c.countdown,
                        c.target_slot,
                        c.found
                    );
                }
                return ExitCode::SUCCESS;
            }

            #[cfg(feature = "window")]
            if play {
                let setup = match load_setup(&dir) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("cannot draw the selector: {e}");
                        return ExitCode::from(5);
                    }
                };
                let win = match open_window(scale) {
                    Ok(w) => w,
                    Err(code) => return code,
                };
                let mut sel = Selector::new();
                sel.island = opts.island;
                sel.launch = usize::from(opts.launch);
                sel.players = opts.players - 1;
                if opts.obj_clock {
                    crate::note!("antics: item placement seeded from the DOS clock (int 21h ah=2ch), \
                              sampled once per round — a fresh layout on every GO! and every \
                              restart; `--seed N` pins it");
                }
                let start_in_round = true; // --scene round --play goes straight to the round
                let dump = match SoundDump::new(sound_dump_path.as_ref()) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                };
                // **stage 24a** — the panel's timeout needs the demo stream; a missing
                // `DEMO.DAT` disarms it rather than starting a live round instead.
                let demo_stream = match load_demo_stream(&dir) {
                    Ok(v) => v,
                    Err(e) => {
                        // a missing DEMO.DAT only disarms the panel's idle timeout: a diagnostic,
                        // not a failure, so it is `--verbose` material (v0.1.2)
                        crate::note_err!("antics: {e} — the panel's idle timeout is inert");
                        Vec::new()
                    }
                };
                if demo_stream.is_empty() && panel_auto {
                    crate::note!("antics: no demo stream — a finished round will wait on the panel");
                }
                return match play_app(win, &setup, sel, opts, exe, marker, start_in_round,
                                      sound_on, panel_auto, demo_stream, dump) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("{e}");
                        ExitCode::from(7)
                    }
                };
            }
            #[cfg(not(feature = "window"))]
            if play {
                eprintln!("this build has no window support (cargo feature `window` off)");
                return ExitCode::from(7);
            }

            let dump = match SoundDump::new(sound_dump_path.as_ref()) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            };
            return match run_engine(&opts, exe, steps, upto, shot, state, shot_every, marker,
                                    engine_ds, lift_blocks, check_engine, strip_out,
                                    tick_trace_path, layout, dump_objects, state_every,
                                    levels_out, retreat, dump, demo_ds) {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::from(6)
                }
            };
        }
        // The round view: a pure function of (spy.exe, one 64 KB data-segment dump).
        let ds = match ds_path {
            Some(p) => p,
            None => {
                eprintln!("--scene round needs --ds FILE (a 65536-byte data-segment dump), or \
                           --play / --script FILE for the engine");
                return ExitCode::from(2);
            }
        };
        let cli = round::RoundCli {
            exe: dir.join("spy.exe").display().to_string(),
            ds: ds.display().to_string(),
            shot: shot.map(|p| p.display().to_string()),
            state: state.map(|p| p.display().to_string()),
            check,
            both,
            layers: round::Layers { objects, spies },
        };
        return match round::run(&cli) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(6)
            }
        };
    }

    // The selector — and, for a plain "play", the screen the intro hands over to (`intro_play`
    // enters this branch too: the window, the options and `play_app` all live here).
    if scene == "setup" || intro_play {
        let dir = dirs.originals.clone();
        // **stage 30** — a no-argument run opens on the intro, so its files are checked first, with
        // the intro's own message when one is missing.
        #[cfg(feature = "window")]
        let intro_engine = if intro_play {
            match intro::Intro::load(&dir) {
                Ok(i) => Some(i),
                Err(e) => {
                    // **v0.1.3** — `fatal` (stderr as before, plus a box for a Windows GUI launch)
                    if dir.join("TITLE.DAT").is_file() {
                        fatal(&format!("cannot run the intro engine: {e}"));
                    } else {
                        fatal_telling(&install::not_found_looking_in(&[&dir]),
                                      &install::not_found_gui(&[&dir]));
                    }
                    return ExitCode::from(3);
                }
            }
        } else {
            None
        };
        // the frame is drawn from `spy.exe` + `TRAP.PIC` by a port of the original's own draw list
        // (`src/selector_draw.rs`); the port reads no art of its own (stage 29)
        let setup = match load_setup(&dir) {
            Ok(s) => s,
            Err(e) => {
                // **v0.1.3** — "cannot draw the selector" is fatal for a player too: boxed.
                fatal_telling(
                    &format!("{}\n(the selector is drawn from spy.exe + TRAP.PIC: {e})",
                             install::not_found_looking_in(&[&dir])),
                    &format!("antics could not draw the setup screen.\n\n{}",
                             install::not_found_gui(&[&dir])),
                );
                return ExitCode::from(5);
            }
        };

        // A key script drives the selector exactly as the original does, so the highlight/change
        // cycle is exercised rather than asserted. `--upto N` replays only the first N keys,
        // which keeps the port a pure function of (assets, key prefix) -- ADR 0002.
        let mut sel = Selector::new();
        sel.cursor = cursor;
        sel.island = island;
        sel.launch = launch;
        sel.players = players;
        sel.difficulty = difficulty;
        if go {
            sel.cursor = 8;
            sel.started = true;
        }
        #[cfg(feature = "window")]
        if play {
            let exe_dir = dirs.originals.clone();
            let exe = match fs::read(exe_dir.join("spy.exe")) {
                Ok(b) => b,
                Err(e) => {
                    // **v0.1.3** — missing `spy.exe` on the play path: boxed.
                    let path = exe_dir.join("spy.exe").display().to_string();
                    fatal_telling(
                        &format!("{path}: {e}"),
                        &format!(
                            "antics could not read spy.exe from your copy of the game.\n\n\
                             {path}\n{e}\n\n\
                             Run the antics installer again, or choose your game folder in it."
                        ),
                    );
                    return ExitCode::from(3);
                }
            };
            // **v0.1.3 (Windows)** — the game data is in hand and the window is the next thing to
            // appear, so this is where the console goes. After it, `fatal`'s stderr write has
            // nowhere to land, which is why it arms the message box; with `--verbose` the console
            // stays, because the player asked to read it.
            if plan.detach_before_window {
                detach_console();
            }
            let mut win = match open_window(scale) {
                Ok(w) => w,
                Err(code) => return code,
            };
            // **stage 30** — the original's own order: the `spytitl.exe` intro first, then this
            // selector. Return/Space/Escape or a click skips it; closing the window quits.
            if let Some(engine) = intro_engine {
                match play_intro(&mut win, engine,
                                 frame_ticks_opt.unwrap_or(game::FRAME_TICKS)) {
                    Ok(IntroFlow::Quit) => return ExitCode::SUCCESS,
                    Ok(_) => {}
                    Err(e) => {
                        // **v0.1.3** — after the detach this stderr write lands nowhere on a
                        // Windows GUI launch, so the failure is boxed as well.
                        fatal(&format!("{e}"));
                        return ExitCode::from(7);
                    }
                }
            }
            let opts = game::RoundOpts {
                difficulty: sel.difficulty as u16,
                cpu: [cpu1, cpu2],
                island: sel.island,
                players: (sel.players + 1).clamp(1, 2),
                seed,
                obj_state,
                // stage 22c: `CS:20cd`'s clock-seeded branch, for the interactive round only
                obj_clock,
                obj_clock_dx: None,
                random_start,
                start_screen,
                start_x,
                start_x2,
                start_y,
                clock_override,
                void_solid,
                igloo_entry,
                no_storm,
                visitor_draw,
                visitor_eval_mover_only,
                spy_block,
                objects,
                spies,
                items_ui,
                launch: sel.launch != 0,
                start_hatch,
                carry,
                carry2,
                carry_at: carry_at.clone(),
                heat,
                heat2,
                parts,
                frame_ticks: frame_ticks_opt,
                demo: None,
            };
            if opts.obj_clock {
                crate::note!("antics: item placement seeded from the DOS clock (int 21h ah=2ch), \
                          sampled once per round — a fresh layout on every GO! and every \
                          restart; `--seed N` pins it");
            }
            let start_in_round = go;
            let dump = match SoundDump::new(sound_dump_path.as_ref()) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            };
            // **stage 24a** — `DEMO.DAT` sits with `spy.exe`, not with the setup art.
            let demo_stream = match load_demo_stream(&exe_dir) {
                Ok(v) => v,
                Err(e) => {
                    // see above: disarming the panel's timeout is a diagnostic (v0.1.2)
                    crate::note_err!("antics: {e} — the panel's idle timeout is inert");
                    Vec::new()
                }
            };
            return match play_app(win, &setup, sel, opts, exe, marker, start_in_round,
                                  sound_on, panel_auto, demo_stream, dump) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    // **v0.1.3** — same reason as the intro's: the console has been detached on a
                    // Windows GUI launch, so a fatal error at the end of play is boxed too.
                    fatal(&format!("{e}"));
                    ExitCode::from(7)
                }
            };
        }
        #[cfg(not(feature = "window"))]
        if play {
            // **v0.1.3** — boxed for the same reason: a GUI launch has no one reading stderr.
            fatal_telling(
                "this build has no window support (cargo feature `window` off)",
                "This copy of antics was built without its window, so it cannot play the game.\n\n\
                 Download the normal release for Windows from the releases page.",
            );
            return ExitCode::from(7);
        }
        if let Some(script) = keys {
            let mut trace = String::new();
            let mut applied = 0usize;
            for tok in script.split([',', ' ']).filter(|t| !t.is_empty()) {
                if let Some(limit) = upto {
                    if applied >= limit {
                        break;
                    }
                }
                sel.key(&tok.to_ascii_lowercase());
                applied += 1;
                trace.push_str(&sel.json().trim_end().to_string());
                trace.push('\n');
            }
            if let Some(p) = &trace_path {
                if let Err(e) = fs::write(p, &trace) {
                    eprintln!("{}: {e}", p.display());
                    return ExitCode::from(4);
                }
            }
        }

        let screen = render_setup(&setup, sel.island, sel.launch, sel.players,
                                  sel.difficulty, sel.cursor);
        if let Some(p) = shot {
            if let Err(e) = write_bmp(&p, &screen, &PALETTE_1) {
                eprintln!("{e}");
                return ExitCode::from(4);
            }
        }
        if let Some(p) = state {
            // GO! is cursor zone 8. Pressing RETURN there commits the choices and starts the
            // round; the round itself is milestone 3, so all that can honestly be reported here
            // is that the selector acted on it.
            if let Err(e) = fs::write(&p, sel.json()) {
                eprintln!("{}: {e}", p.display());
                return ExitCode::from(4);
            }
        }
        return ExitCode::SUCCESS;
    }

    // The intro: the original's own script machine, run over the player's files. No measured art
    // is read -- `spytitl.exe`, `TITLE.DAT`, `MAXX.PIC` and `TEXT1..3.DAT` are the whole input.
    let dir = dirs.originals.clone();
    let mut engine = match intro::Intro::load(&dir) {
        Ok(i) => i,
        Err(e) => {
            if dir.join("TITLE.DAT").is_file() {
                eprintln!("cannot run the intro engine: {e}");
            } else {
                eprintln!("{}", install::not_found_looking_in(&[&dir]));
            }
            return ExitCode::from(3);
        }
    };
    // The frame index is the machine's dispatch count: 0 is the MAXX card, 1 the composed scene,
    // and `n` the screen after the n-1-th script byte (1,841 states in all).
    let at = (frame_no as usize).min(intro::LAST_STATE);
    engine.seek(at);
    let screen = Screen { idx: engine.indices() };
    let palette_id = engine.palette();
    let strip = engine.strip();
    let slots = engine.slot_count();

    if let Some(p) = shot {
        let palette = if palette_id == 0 { &PALETTE_0 } else { &PALETTE_1 };
        if let Err(e) = write_bmp(&p, &screen, palette) {
            eprintln!("{e}");
            return ExitCode::from(4);
        }
    }
    if let Some(p) = state {
        let json = format!(
            "{{\"frame\":{},\"scene\":\"{}\",\"slots\":{},\"strip\":{},\"palette\":{}}}\n",
            frame_no,
            if palette_id == 0 { "intro-card" } else { "intro-arctic" },
            slots,
            strip,
            palette_id
        );
        if let Err(e) = fs::write(&p, json) {
            eprintln!("{}: {e}", p.display());
            return ExitCode::from(4);
        }
    }
    ExitCode::SUCCESS
}

// ---- tests ---------------------------------------------------------------
#[cfg(test)]
mod selector_tests {
    use super::*;

    /// **The owner's item 4.** The setup screen's launch zone (`DS:0x2EC0` zone 2) must reach the
    /// round: on SHOW the hatch is level content from the placement on (`CS:269d`), and on HIDE it
    /// only appears when the briefcase holds all three parts (`CS:0617`). Dropping the flag here
    /// made every round started from the selector a HIDE round, whatever the screen showed.
    #[test]
    fn a_round_started_from_the_selector_keeps_the_launch_site() {
        let mut sel = Selector::new();
        sel.island = 3;
        sel.players = 1;
        sel.launch = 1;
        let o = opts_for_selector(&game::RoundOpts::default(), &sel);
        assert!(o.launch, "SHOW reaches the round");
        assert_eq!(o.island, 3);
        assert_eq!(o.players, 2);

        // and HIDE wins over a command line that asked for SHOW: the selector is the last word
        let mut base = game::RoundOpts::default();
        base.launch = true;
        let mut hide = Selector::new();
        hide.launch = 0;
        assert!(!opts_for_selector(&base, &hide).launch);
        assert!(opts_for_selector(&base, &sel).launch);
        // everything else is inherited from the command line
        let mut base2 = game::RoundOpts::default();
        base2.objects = false;
        assert!(!opts_for_selector(&base2, &sel).objects);
    }

    /// **The difficulty zone is wired, not just drawn.** The options screen's zone steps the value
    /// 0..4 (`CS:1066` writes `DS:0x31BF` = 2 * value), and the round reads that word itself, so
    /// the selector's choice must reach `RoundOpts` and from there `DS:0x31BF`.
    #[test]
    fn the_selector_difficulty_reaches_the_round() {
        let mut sel = Selector::new();
        assert_eq!(sel.difficulty, 0, "the options screen starts on difficulty 1");
        sel.key("down");            // cursor 0 -> 2 (the launch site)
        sel.key("right");           // ... -> 6, the difficulty zone
        assert_eq!(sel.cursor, 6);
        for _ in 0..4 {
            sel.key("return");
        }
        assert_eq!(sel.difficulty, 4, "five settings, 1..5, then back to 1");
        sel.key("return");
        assert_eq!(sel.difficulty, 0, "and the zone wraps");
        sel.difficulty = 4;
        let o = opts_for_selector(&game::RoundOpts::default(), &sel);
        assert_eq!(o.difficulty, 4, "the zone's value reaches the round's options");
        // and the word the round carries is 2 * that (`CS:1066`'s own step)
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = std::fs::read(crate::orig::require("spy-vs-spy-iii/original/256/spy.exe"))
            .expect("spy.exe");
        let g = game::Game::new(exe, &o).expect("round");
        assert_eq!(g.difficulty, 4);
        let st = g.round_state();
        assert_eq!(u16::from_le_bytes([st.ds[round::DS_DIFF], st.ds[round::DS_DIFF + 1]]), 8,
                   "difficulty 5 is 0x31BF = 8, as rocket-room-4-hard records");
    }
    /// **stage 23c — the between-rounds panel starts the round by itself** (`CS:0ED1`).
    ///
    /// The panel *is* the setup selector: after a round the game re-enters `FUN_26c0_0440`, puts the
    /// selector up (`CS:046F`), and runs `FUN_26c0_0ED1` in place of the round — one panel frame
    /// every 8 ticks of the 240 Hz PIT (`CS:0F0D`), and at **1000** frames (`CS:0F1E`) it forces the
    /// site to SHOW (`CS:0F32`), sets `DS:0x3203` (`CS:0F38`) and loads the island.
    ///
    /// **Stage 24a (owner correction, 2026-10-30):** what `0x3203` starts is the **attract demo**,
    /// not a live round — `CS:0325` takes its `CS:0203` branch and replays `DEMO.DAT` for both spies
    /// (see [`demo_opts`]). So the timeout sets the *demo* flag, leaves `started` alone (that is the
    /// RETURN path), and does not touch the selector's cursor — the original's timeout writes only
    /// `0x2F7C`, `0x31BB`, `0x31D7`, `0x3203`, `0x31B0` and `0x0FB6`.
    #[test]
    fn the_panel_timeout_enters_the_attract_demo_not_a_live_round() {
        let mut sel = Selector::new();
        sel.difficulty = 3;
        sel.launch = 0;
        sel.island = 5;
        sel.cursor = 2;
        let mut fired_at = None;
        for n in 1..=1500u32 {
            if sel.panel_frame() {
                fired_at = Some(n);
                break;
            }
        }
        assert_eq!(fired_at, Some(PANEL_FRAMES_TO_START), "the timeout fires at 1000");
        assert!(sel.demo, "`0x3203` is set: the next pass takes the attract branch");
        assert!(!sel.started, "and NOT the RETURN path: a demo is not a round he chose");
        assert_eq!(sel.island, 0, "`CS:0F2C` forces island 1 whatever the panel showed");
        assert_eq!(sel.launch, 1, "`CS:0F32` forces the site to SHOW");
        assert_eq!(sel.cursor, 2, "the timeout writes no cursor: `0x2EC0` is untouched");
        assert_eq!(sel.difficulty, 3, "and no difficulty: `0x31BF` is untouched");
        // and a key-driven GO! is still a GO!: the counter never fires if he presses RETURN first
        let mut pressed = Selector::new();
        pressed.cursor = 8;
        pressed.key("return");
        assert!(pressed.started);
        assert!(!pressed.demo, "RETURN starts a round, not the demo");
        assert_eq!(pressed.launch, 0, "the key path does not force SHOW — only the timeout does");
    }

    /// **Stage 24a** — the round the timeout builds: `CS:0F26..0x0F4A`'s six writes, in the port's
    /// terms, plus the demo stream the caller hands in.
    #[test]
    fn demo_opts_are_the_timeouts_six_writes() {
        let mut base = game::RoundOpts::default();
        base.island = 4;          // the panel was showing island 5
        base.launch = false;      // ... and HIDE
        base.difficulty = 4;      // difficulty 5: the panel's, which the timeout never touches
        base.cpu = [false, true]; // the one-player option
        base.obj_state = Some(0x1234);
        base.obj_clock = true;
        let stream: Vec<u16> = vec![0, 0x0004, 0x0300];
        let o = demo_opts(&base, &stream);
        assert_eq!(o.island, 0, "`CS:0F2C`: [0x31BB] = 0");
        assert!(o.launch, "`CS:0F32`: [0x31D7] = 2 -> SHOW");
        assert_eq!(o.players, 2, "both spies: [0x31DD] is never read on the attract path");
        assert_eq!(o.cpu, [false, false], "`CS:0203` never reaches `CS:129C`");
        assert_eq!(o.obj_state, Some(round::RNG_ATTRACT_SEED), "`CS:0F3E`: [0x31B0] = 0x469");
        assert!(!o.obj_clock, "and DX stays the fixed 0x770D");
        assert_eq!(o.demo.as_deref(), Some(stream.as_slice()), "`CS:0F44`: [0x0FB6] = 0");
        assert_eq!(o.difficulty, 4, "the difficulty is the panel's own");
    }

    /// **Stage 24a** — a finished demo lands on the panel with the wait restarted.
    ///
    /// `CS:06D8` is the round's exit for every ending (the clock, the rocket, both spies out) *and*
    /// for Escape: it silences the speaker and jumps to `0x60D`, which re-enters `0x440` -> `0xED1`
    /// -> the panel. `0xED1` clears `0x3203`, so the panel is a live panel again, and `0x0F26` has
    /// already set `0x2F7C` to 0 — so the next timeout is a full 33 s away. Escape during the demo is
    /// the same exit as Escape during a round (`0x33E2` is the key handler's own flag, not the
    /// input scan's, so `CS:0203` never clears it).
    #[test]
    fn a_finished_demo_lands_on_the_panel_with_the_wait_restarted() {
        let mut sel = Selector::new();
        while !sel.panel_frame() {}
        assert!(sel.demo && !sel.started);
        // the round runs and ends; `AppScreen::Round`'s exit path runs this:
        panel_entry(&mut sel);
        assert!(!sel.demo, "`CS:0ED1` clears `0x3203`: the panel is a live panel again");
        assert!(!sel.started);
        assert_eq!(sel.panel_frames, 0, "`CS:0F26` left the counter at 0");
        // so the next timeout needs another full 1000 panel frames
        let mut n = 0u32;
        while !sel.panel_frame() {
            n += 1;
            assert!(n < PANEL_FRAMES_TO_START);
        }
        assert_eq!(n + 1, PANEL_FRAMES_TO_START, "a full wait again");
    }

    /// **The panel's rate is the recorded one** — `CS:0F0D` waits for `0x33F4 >= 8` ticks of the
    /// 240 Hz PIT, i.e. **30 frames a second**, so 1000 frames is 33.3 s. Measured in `playtest3`:
    /// `DS:0x2F7C` ran 438 -> 888 in 15.0 s, exactly 30.0 a second.
    #[test]
    fn the_panel_frame_rate_is_the_recorded_thirty_a_second() {
        assert_eq!(game::TICK_HZ / SETUP_REDRAW as u32, 30, "8 ticks of the 240 Hz PIT is 30 Hz");
        let secs = PANEL_FRAMES_TO_START as f64 * SETUP_REDRAW as f64 / game::TICK_HZ as f64;
        assert!((secs - 33.33).abs() < 0.05, "1000 panel frames = {secs} s, not 33.3");
        assert_eq!(PANEL_FRAMES_TO_START, 1000, "`CS:0F1E` compares with 0x3E8");
    }

    /// **The counter carries, and the dwell is `1000 - carried`** — the measured panel
    /// (`playtest3` ds_00244, `0x2F7C = 438`, dwell 18.7 s) is `(1000 - 438) / 30 = 18.73` s, and the
    /// difficulty step is the one thing besides the timeout that clears it (`CS:1080`).
    #[test]
    fn the_panel_counter_carries_and_sets_the_dwell() {
        let mut sel = Selector::new();
        sel.panel_frames = 438;
        let mut n = 0u32;
        while !sel.panel_frame() {
            n += 1;
            assert!(n <= 1000, "it must start within 1000 frames");
        }
        assert_eq!(n + 1, PANEL_FRAMES_TO_START - 438, "562 panel frames from 438");
        let secs = (PANEL_FRAMES_TO_START - 438) as f64 / PANEL_HZ as f64;
        assert!((secs - 18.7).abs() < 0.05, "(1000-438)/30 = {secs} s — the recorded 18.7 s");
        // a fresh panel (a session's first) waits the full 33.3 s
        let mut fresh = Selector::new();
        let mut m = 0u32;
        while !fresh.panel_frame() {
            m += 1;
        }
        assert_eq!(m + 1, PANEL_FRAMES_TO_START);
        assert!((PANEL_FRAMES_TO_START as f64 / PANEL_HZ as f64 - 33.33).abs() < 0.05);
        // `CS:1080`: changing the difficulty restarts the wait
        let mut d = Selector::new();
        d.panel_frames = 900;
        d.key("down"); // zone 0 -> 2
        d.key("right"); // -> 6, the difficulty row
        d.key("return");
        assert_eq!(d.panel_frames, 0, "the difficulty step clears the counter");
    }

    /// **Stage 29 — the selector's frame is the drawn one, and it is the tracked picture.**
    ///
    /// `load_setup` takes the resolved data directory and needs nothing but `spy.exe` and
    /// `TRAP.PIC`. Its default frame must equal the tracked `setup_bg.bin` (the captured selector
    /// with the rings lifted out) on every one of the 64,000 pixels — that file is this checkout's
    /// oracle, never an input: the engine has no loader for it.
    #[test]
    fn the_selector_frame_is_the_drawn_panel() {
        if crate::orig::absent() {
            crate::orig::skip("the selector's two files");
            return;
        }
        let orig_dir = crate::orig::require("spy-vs-spy-iii/original/256");
        let setup = load_setup(&orig_dir).expect("the drawn selector");
        let got = render_setup(&setup, 0, 0, 0, 0, 8);
        assert_eq!(got.idx.len(), PIXELS);

        let Some(bg) = crate::orig::oracle("setup_bg.bin") else {
            crate::orig::skip("assets/setup_bg.bin (the selector oracle)");
            return;
        };
        // the default state with no ring is the ring-less frame `setup_bg.bin` stores
        let plain = render_setup(&setup, 0, 0, 0, 0, 8);
        let mut ring_diff = 0usize;
        for i in 0..PIXELS {
            if plain.idx[i] != bg[i] {
                ring_diff += 1;
            }
        }
        // cursor 8 is the GO! ring, which the oracle does not carry: compare the ring-less frame
        let ringless = Screen { idx: setup.bg.idx.clone() };
        let n = ringless.idx.iter().zip(&bg).filter(|(a, b)| a != b).count();
        assert_eq!(n, 0, "{n} px of the ring-less selector differ from setup_bg.bin");
        assert!(ring_diff > 0, "the GO! ring must be drawn on top of the ring-less frame");
    }

    /// **A directory without the two files is refused, by name.** That is what a player with no
    /// installed set sees, and the message has to say which file is missing.
    #[test]
    fn the_selector_needs_spy_exe_and_trap_pic() {
        let empty = std::env::temp_dir().join(format!("antics-nosel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&empty);
        let err = match load_setup(&empty) {
            Ok(_) => panic!("an empty directory has no selector"),
            Err(e) => e,
        };
        assert!(err.contains("spy.exe") && err.contains("TRAP.PIC"), "{err}");
        let _ = std::fs::remove_dir(&empty);
    }

    /// **The panel's five difficulty levels and its two signs are five and two different pictures**,
    /// each in its own cell of the bottom strip — `DS:0x31BF / 2 + 1` (levels 1..5) and
    /// `DS:0x31D7` (0 = HIDE, 2 = SHOW), drawn by the original's own code.
    #[test]
    fn the_panel_shows_five_difficulty_levels_and_two_signs() {
        if crate::orig::absent() {
            crate::orig::skip("the selector's two files");
            return;
        }
        let dir = crate::orig::require("spy-vs-spy-iii/original/256");
        let setup = load_setup(&dir).expect("the drawn selector");
        let bbox = |a: &Screen, b: &Screen| -> (usize, usize, usize, usize) {
            let (mut x0, mut y0, mut x1, mut y1) = (W, H, 0usize, 0usize);
            for i in 0..PIXELS {
                if a.idx[i] != b.idx[i] {
                    let (x, y) = (i % W, i / W);
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
            assert!(x0 <= x1 && y0 <= y1, "the two frames are identical");
            (x0, y0, x1, y1)
        };
        let base = render_setup(&setup, 0, 0, 0, 0, 0);
        let mut cells = Vec::new();
        for d in 1..5usize {
            let f = render_setup(&setup, 0, 0, 0, d, 0);
            let r = bbox(&base, &f);
            assert!(r.1 >= selector_draw::BLIT_ROW[1],
                    "the difficulty digit is in the bottom strip, not at {r:?}");
            cells.push(r);
        }
        // one cell for all five: the segment records are <= 3 bytes wide and 12 rows tall, drawn
        // with their bottom row at `DIFFICULTY_BOTTOM` and their left column at `DIFFICULTY_COL`
        // (both measured from the strip's own top-left, x = (BLIT_COL + col) * 4)
        let cell = (
            (selector_draw::BLIT_COL + selector_draw::DIFFICULTY_COL) * 4,
            selector_draw::BLIT_ROW[1] + selector_draw::DIFFICULTY_BOTTOM - 12,
            (selector_draw::BLIT_COL + selector_draw::DIFFICULTY_COL + 3) * 4,
            selector_draw::BLIT_ROW[1] + selector_draw::DIFFICULTY_BOTTOM,
        );
        for (d, r) in cells.iter().enumerate() {
            assert!(
                r.0 >= cell.0 && r.1 >= cell.1 && r.2 < cell.2 && r.3 <= cell.3,
                "level {} moved {r:?}, outside the digit's own cell {cell:?}", d + 2
            );
        }
        // the five levels are five distinct digits
        let frames: std::collections::BTreeSet<Vec<u8>> = (0..5usize)
            .map(|d| render_setup(&setup, 0, 0, 0, d, 0).idx)
            .collect();
        assert_eq!(frames.len(), 5, "five difficulty levels, five digits");

        // ... and HIDE / SHOW is a different cell from the digit
        let site = bbox(&base, &render_setup(&setup, 0, 1, 0, 0, 0));
        assert!(site.2 < cells[0].0 || site.0 > cells[0].2,
                "the sign {site:?} and the digit {:?} share a cell", cells[0]);
        assert!(site.1 >= selector_draw::BLIT_ROW[1], "the sign is in the bottom strip");

        // and the selector's own words: `launch` 0 = HIDE, 1 = SHOW; difficulty 0..4 = levels 1..5
        let mut sel = Selector::new();
        sel.launch = 1;
        sel.difficulty = 4;
        let o = opts_for_selector(&game::RoundOpts::default(), &sel);
        assert!(o.launch, "SHOW reaches the round as `0x31D7 = 2`");
        assert_eq!(o.difficulty, 4, "level 5 is `0x31BF = 8`");
    }

    /// **A round that ends returns to the panel, not to a fresh selector** — the same island, site,
    /// difficulty and player count, and the panel's counter restarted (`CS:0F26` clears `0x2F7C`
    /// on the timeout; the port resets it on entry).
    #[test]
    fn a_round_that_ends_returns_to_the_panel_not_a_fresh_selector() {
        let mut sel = Selector::new();
        sel.island = 5;
        sel.launch = 1;
        sel.players = 1;
        sel.difficulty = 4;
        sel.started = true;
        sel.panel_frames = 438;
        // the round ends: `CS:060D` -> `CS:0440` -> `CS:046F` (the entry) and the panel is up again
        panel_entry(&mut sel);
        assert!(!sel.started, "GO! is consumed by the round it started");
        assert_eq!(sel.panel_frames, 438,
                   "the entry does not touch the counter — it carries (playtest3 entered at 438)");
        assert_eq!((sel.island, sel.launch, sel.players, sel.difficulty), (5, 1, 1, 4),
                   "every choice survives the round");
        let o = opts_for_selector(&game::RoundOpts::default(), &sel);
        assert_eq!((o.island, o.launch, o.players, o.difficulty), (5, true, 2, 4));
    }

    /// **The panel drops what the spies were carrying** (`CS:04D4`/`0x04EC`: `0x2F2C = 0x0C` on both,
    /// "nothing in hand"). The port drops it by starting a fresh round — the panel holds no spy state
    /// — so what this pins is that nothing survives from the finished round into the next one.
    #[test]
    fn the_panel_drops_what_the_spies_were_carrying() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let exe = std::fs::read(crate::orig::require("spy-vs-spy-iii/original/256/spy.exe"))
            .expect("spy.exe");
        let mut sel = Selector::new();
        sel.started = true;
        let o = opts_for_selector(&game::RoundOpts::default(), &sel);
        let mut g = game::Game::new(exe.clone(), &o).expect("round");
        for i in 0..2 {
            g.spies[i].carried = 2; // dynamite in hand
            g.spies[i].dyn_sticks = 3;
            g.spies[i].kit |= 0x3F;
        }
        panel_entry(&mut sel);
        let o2 = opts_for_selector(&game::RoundOpts::default(), &sel);
        let g2 = game::Game::new(exe, &o2).expect("next round");
        for i in 0..2 {
            assert_eq!(g2.spies[i].carried, game::CARRY_NONE_UI, "spy {} starts empty-handed", i + 1);
            assert_eq!(g2.spies[i].dyn_sticks, 0);
        }
    }

    /// **The flag policy** — the auto-start is on for the interactive round and off for every
    /// headless path, and `--panel-auto on|off` overrides it either way.
    #[test]
    fn the_panel_auto_start_is_opt_in() {
        assert!(!panel_auto_enabled(None, false, false), "headless: off");
        assert!(!panel_auto_enabled(None, true, true), "`--go` starts in the round: off");
        assert!(!panel_auto_enabled(None, true, false), "--play: off by default (owner, 2026-10-29)");
        assert!(!panel_auto_enabled(Some(false), true, false), "--panel-auto off");
        assert!(panel_auto_enabled(Some(true), true, false), "--panel-auto on arms it");
    }

    /// **stage 23c — the panel's BODY HEAT gauges are full whatever the heat word says.**
    ///
    /// On the briefing/panel screen the two gauges are *part of the picture*, not a readout: nothing
    /// on that screen reads `DS:0x2F58`/`0x2F5A`. `rois.toml` carries the negative twice — of the
    /// 4 recorded panel dumps in `m3-heat-90s`, the two whose heat word had drifted to 9 still read
    /// the full bar (59 of 59 rows), while all 176 running-clock dumps follow the word.
    ///
    /// The port satisfies this by construction: the panel is `AppScreen::Setup` -> `render_setup`,
    /// which paints the captured selector art and never calls `Game::draw_heat_bars`. This test
    /// pins the two claims that keep it true:
    ///
    /// 1. the panel's gauge region is **byte-identical** to the round's own gauge at a full bar, so
    ///    nothing about the panel's picture differs from a full bar by even one pixel;
    /// 2. the round's gauge *does* follow the word (an empty bar differs), so claim 1 is not
    ///    vacuous — if the panel were ever routed through `Game::frame()`, the empty-bar frame would
    ///    paint 522 drained pixels over it and this test would fail on 1.
    #[test]
    fn the_panel_draws_both_gauges_full_whatever_the_heat() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let dir = crate::orig::require("spy-vs-spy-iii/original/256");
        let setup = load_setup(&dir).expect("the drawn selector");
        let panel = render_setup(&setup, 0, 0, 0, 0, 0);
        let exe = std::fs::read(dir.join("spy.exe")).expect("spy.exe");

        let mut o = game::RoundOpts::default();
        o.clock_override = Some(60);
        o.heat = Some(game::HEAT_FULL); // a full bar
        let full = game::Game::new(exe.clone(), &o).expect("round").frame(false);
        o.heat = Some(game::HEAT_FREEZE_AT); // the last survivable value: a nearly empty bar
        let empty = game::Game::new(exe, &o).expect("round").frame(false);

        let mut full_diff = 0;
        let mut empty_diff = 0;
        for y0 in [game::HEAT_BAR_TOP_Y, game::HEAT_BAR_BOTTOM_Y] {
            for r in 0..game::HEAT_BAR_ROWS {
                for c in 0..game::HEAT_BAR_W {
                    let i = (y0 + r) * W + game::HEAT_BAR_X + c;
                    if panel.idx[i] != full.idx[i] {
                        full_diff += 1;
                    }
                    if panel.idx[i] != empty.idx[i] {
                        empty_diff += 1;
                    }
                }
            }
        }
        assert_eq!(full_diff, 0,
                   "the panel's two gauges are the full bar, pixel for pixel ({full_diff} differ)");
        assert!(empty_diff > 400,
                "and the round's own gauge follows the word ({empty_diff} of 1062 differ)");
        // the panel cannot drift with the round's heat because it takes no round state at all
        let again = render_setup(&setup, 0, 0, 0, 0, 0);
        assert_eq!(panel.idx, again.idx);
        // `draw_heat_bars` is where a routed panel would break it: with `Phase::Over` the frame is
        // never drawn (the port shows the selector instead), so no guard is needed there — this
        // assertion records that the panel is not drawn by it.
        assert!(game::HEAT_BAR_ROWS == 59 && game::HEAT_BAR_W == 9);
    }

    /// **stage 22c — the flag policy.** The interactive round gets the original's clock-seeded
    /// placement; every headless path stays a pure function of its arguments, and `--seed` /
    /// `--obj-state` still pin the layout even in play.
    #[test]
    fn the_clock_seeded_placement_is_on_for_play_only() {
        // headless: off, always
        assert!(!obj_clock_enabled(false, false, false, None));
        assert!(!obj_clock_enabled(false, false, true, None));
        // play: on by default
        assert!(obj_clock_enabled(false, true, false, None));
        // ...unless the caller pinned the layout
        assert!(!obj_clock_enabled(false, true, true, None), "--seed N pins the layout");
        assert!(!obj_clock_enabled(false, true, false, Some(0x0469)),
                "--obj-state pins the layout (the attract replay)");
        assert!(!obj_clock_enabled(false, true, true, Some(0x0007)));
        // an explicit --obj-clock is the caller's own choice and wins over everything
        assert!(obj_clock_enabled(true, false, false, None));
        assert!(obj_clock_enabled(true, false, true, None));
        assert!(obj_clock_enabled(true, true, true, Some(0x0469)));
    }
}

/// A word out of a 64 KB data-segment dump.
fn word_at(ds: &[u8], a: usize) -> u16 {
    u16::from_le_bytes([ds[a & 0xFFFF], ds[(a + 1) & 0xFFFF]])
}

/// The six key flags as `U D L R F`, fire being either button.
fn flags(k: &game::Keys) -> String {
    format!(
        "{}{}{}{}{}",
        if k.up { "U" } else { "-" },
        if k.down { "D" } else { "-" },
        if k.left { "L" } else { "-" },
        if k.right { "R" } else { "-" },
        // **stage 18f** — the recorded string below folds the two buttons (`flag(0) || flag(1)`),
        // because the game's own frame reads A and B and the corpus does not distinguish them; the
        // port's `fire_b` is the computer's B, so it folds in the same way.
        if k.fire || k.fire_b { "F" } else { "-" }
    )
}

/// The key flags the recording holds for player `i` at this dump, read **through the pointer table**
/// at `0x327F` (twelve words: button A, button B, up, down, left, right — two per flag, player 1
/// then player 2). The two players' words live in different places (`0x33BE`/`0x33B2` for A and
/// `0x3271..`/`0x3265..` for the rest), so indexing them by a fixed offset per player is wrong.
fn recorded_keys(ds: &[u8], i: usize) -> String {
    let flag = |k: usize| word_at(ds, word_at(ds, 0x327f + 4 * k + 2 * i) as usize) != 0;
    format!(
        "{}{}{}{}{}",
        if flag(2) { "U" } else { "-" },
        if flag(3) { "D" } else { "-" },
        if flag(4) { "L" } else { "-" },
        if flag(5) { "R" } else { "-" },
        if flag(0) || flag(1) { "F" } else { "-" }
    )
}

/// **Stage 29 — a data directory made only by `--install` runs every scene.**
///
/// The end-to-end proof that a player never needs anything of ours: copy the nine files into a
/// scratch directory exactly as `--install` does, then render the intro, the selector and a round
/// from *that* directory and from the development copy, and require the frames to be identical —
/// byte for byte, so their md5s are too.
#[cfg(test)]
mod install_e2e_tests {
    use crate::round::RoundAssets;
    use crate::{game, install, intro};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let p = std::env::temp_dir().join(format!(
                "antics-e2e-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("scratch");
            Scratch(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_installed_data_dir_renders_the_same_intro_selector_and_round() {
        if crate::orig::absent() {
            crate::orig::skip("the original game data");
            return;
        }
        let dev = crate::orig::require(install::ORIGINAL_DEV);
        let out = Scratch::new("installed");
        let rep = install::install(&dev, &out.0).expect("install");
        assert_eq!(rep.copied.len(), 9, "all nine files, and nothing else");

        // 1. the intro: the MAXX card, the composed scene and the sequence's end
        let mut a = intro::Intro::load(&dev).expect("the intro from the development copy");
        let mut b = intro::Intro::load(&out.0).expect("the intro from the installed copy");
        for f in [0usize, 1, 350, 552, 1841] {
            let at = f.min(intro::LAST_STATE);
            a.seek(at);
            b.seek(at);
            assert!(a.indices() == b.indices(), "intro frame {at} differs");
            assert_eq!(a.palette(), b.palette(), "intro palette at {at}");
            assert_eq!(a.strip(), b.strip(), "intro strip at {at}");
            assert_eq!(a.slot_count(), b.slot_count(), "intro slots at {at}");
        }

        // 2. the selector: the ring-less frame and all five rings
        let sa = crate::load_setup(&dev).expect("the selector from the development copy");
        let sb = crate::load_setup(&out.0).expect("the selector from the installed copy");
        for (island, launch, players, difficulty) in
            [(0usize, 0usize, 0usize, 0usize), (3, 1, 1, 2), (6, 1, 0, 4)]
        {
            for cursor in [0u8, 2, 4, 6, 8] {
                let fa = crate::render_setup(&sa, island, launch, players, difficulty, cursor);
                let fb = crate::render_setup(&sb, island, launch, players, difficulty, cursor);
                assert!(
                    fa.idx == fb.idx,
                    "selector island {island} launch {launch} players {players} \
                     difficulty {difficulty} cursor {cursor} differs"
                );
            }
        }

        // 3. a round: same options, same plate, same frame
        let o = game::RoundOpts { island: 0, players: 2, seed: 0, ..game::RoundOpts::default() };
        let exe_a = std::fs::read(dev.join("spy.exe")).expect("spy.exe");
        let exe_b = std::fs::read(out.0.join("spy.exe")).expect("installed spy.exe");
        let mut ga = game::Game::new(exe_a, &o).expect("round");
        let mut gb = game::Game::new(exe_b, &o).expect("round");
        ga.assets = RoundAssets::load_from(&dev);
        gb.assets = RoundAssets::load_from(&out.0);
        assert!(ga.assets.plate.is_some(), "the development copy has TRAP.PIC");
        assert!(gb.assets.plate.is_some(), "the installed copy has TRAP.PIC");
        for frame_no in 0..8usize {
            if frame_no > 0 {
                let step = game::Input::default();
                ga.tick(step);
                gb.tick(step);
            }
            let fa = ga.frame(false);
            let fb = gb.frame(false);
            assert!(fa.idx == fb.idx, "round frame {frame_no} differs");
        }
    }
}

/// **Stage 30 — what the command line asks for.** Pure functions only, so every case here runs on a
/// machine with no display (which is where the v0.1.0 defect showed up worst: `antics` and
/// `antics --play` exited 0 without a window).
#[cfg(test)]
mod launch_tests {
    use super::{default_play, first_headless_flag, intro_dispatch_period, plays_intro,
                verbosity_requested, INTRO_CARD_HOLD_TICKS, INTRO_FRAMES};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// **The owner's first case.** No arguments at all must play, and so must a command line that
    /// only says how to open the window or where the data is.
    #[test]
    fn no_arguments_and_window_only_options_play() {
        assert!(default_play(&args(&[])), "`antics` plays");
        assert!(default_play(&args(&["--play"])), "`antics --play` plays");
        assert!(default_play(&args(&["--scale", "4"])), "`antics --scale 4` plays");
        assert!(default_play(&args(&["--no-sound"])), "sound is a window option");
        assert!(default_play(&args(&["--sound"])), "... in both directions");
        assert!(default_play(&args(&["--data-dir", "/tmp/svs3"])), "so is --data-dir");
        assert!(default_play(&args(&["--assets", "/tmp/svs3"])), "and its --assets spelling");
        assert!(default_play(&args(&["--fps", "10"])), "`--fps` only changes the pacing");
        assert!(default_play(&args(&["--panel-auto", "off"])), "--panel-auto on|off");
        assert!(default_play(&args(&["--marker"])), "the spy marker is a window option");
        assert!(default_play(&args(&["--scale", "2", "--no-sound", "--data-dir", "/d"])),
                "any combination of window/data options still plays");
        // a window option with no value is a parse error (exit 2), not a play command
        assert!(!default_play(&args(&["--scale"])), "--scale needs a value");
        assert!(!default_play(&args(&["--data-dir"])), "--data-dir needs a value");
    }

    /// **Every headless path keeps its own meaning.** A flag that asks for output, or picks a
    /// scene, or is one of the release/verification entry points, is never turned into a play.
    #[test]
    fn headless_flags_are_not_play() {
        for a in [
            vec!["--shot", "f.bmp"],
            vec!["--state", "f.json"],
            vec!["--frame", "350"],
            vec!["--scene", "setup"],
            vec!["--scene", "round"],
            vec!["--scene", "intro"],
            vec!["--script", "s.script"],
            vec!["--tick-trace", "t.txt"],
            vec!["--trace", "t.txt"],
            vec!["--keys", "down,return"],
            vec!["--layout"],
            vec!["--dump-objects"],
            vec!["--demo"],
            vec!["--demo-frames", "10"],
            vec!["--cpu-frames", "5"],
            vec!["--ds", "ds.bin"],
            vec!["--check"],
            vec!["--install", "/tmp/svs3"],
            vec!["--where"],
            // ... and mixed with a window option, the headless flag wins
            vec!["--scale", "4", "--shot", "f.bmp"],
        ] {
            assert!(!default_play(&args(&a)), "{a:?} is a headless command line");
        }
    }

    /// `--install` and `--where` are unchanged: they still run first and then exit, and their names
    /// are what the "why not play" line reports.
    #[test]
    fn install_and_where_keep_their_entry_points() {
        assert_eq!(first_headless_flag(&args(&["--install", "/tmp/sv"])), Some("--install"));
        assert_eq!(first_headless_flag(&args(&["--where"])), Some("--where"));
        assert_eq!(first_headless_flag(&args(&["--scale", "2", "--where"])), Some("--where"));
        assert_eq!(first_headless_flag(&args(&["--play"])), None);
        assert_eq!(first_headless_flag(&args(&[])), None);
        assert_eq!(first_headless_flag(&args(&["--frame", "0"])), Some("--frame"));
    }

    /// **When the run opens on the intro.** Only a plain play does; an explicit scene does not, and
    /// an explicit `--play` next to a headless flag does not either (the caller says so instead of
    /// silently ignoring it).
    #[test]
    fn only_a_plain_play_opens_on_the_intro() {
        assert!(plays_intro(true, "intro", &args(&[])), "`antics`");
        assert!(plays_intro(true, "intro", &args(&["--play", "--scale", "2"])), "`--play --scale 2`");
        assert!(plays_intro(true, "intro", &args(&["--no-sound", "--data-dir", "/d"])));
        // an explicit scene never starts on the intro, whatever --play says
        assert!(!plays_intro(true, "setup", &args(&["--scene", "setup", "--play"])));
        assert!(!plays_intro(true, "round", &args(&["--scene", "round", "--play"])));
        // `--play --frame 350` and `--scene intro --play` keep their headless behaviour
        assert!(!plays_intro(true, "intro", &args(&["--play", "--frame", "350"])));
        assert!(!plays_intro(true, "intro", &args(&["--scene", "intro", "--play"])));
        // and no play flag at all is the headless intro, exactly as before stage 30
        assert!(!plays_intro(false, "intro", &args(&[])));
        assert!(!plays_intro(false, "intro", &args(&["--frame", "0", "--shot", "f.bmp"])));
    }

    /// **v0.1.2 — the program is quiet unless asked.** The switch is one process-wide flag, off
    /// until `-v` / `--verbose` is parsed; `note!` (every informational line in the program) reads
    /// it and prints nothing while it is off. So the intro's banner, the window/speaker lines, the
    /// selector banner and the per-event item/room lines cannot reach a terminal that did not ask
    /// for them — which is what the owner saw as "the terminal prints what is happening all the
    /// time".
    ///
    /// This test owns the flag for its whole life (it starts and ends off), so it asserts the
    /// default as well as the two spellings.
    #[test]
    fn nothing_informational_is_printed_unless_asked() {
        // the default a released binary starts with
        assert!(!crate::verbose::on(), "the chatter defaults to off");
        let quiet = crate::verbose::note_count();
        crate::note!("there are {} states in the intro", INTRO_FRAMES);
        assert_eq!(crate::verbose::note_count(), quiet, "`note!` printed while off");

        // `-v` and `--verbose` both ask for it, wherever they sit on the line
        assert!(verbosity_requested(&args(&["-v"])), "`antics -v`");
        assert!(verbosity_requested(&args(&["--verbose"])), "`antics --verbose`");
        assert!(verbosity_requested(&args(&["--play", "-v"])));
        assert!(verbosity_requested(&args(&["-v", "--scene", "setup"])));
        assert!(!verbosity_requested(&args(&[])), "no flag, no chatter");
        assert!(!verbosity_requested(&args(&["--play", "--scale", "2"])));
        assert!(!verbosity_requested(&args(&["--verbose=1"])), "it takes no value");

        // and with it on, the same note is emitted (the old output, line for line)
        crate::verbose::set(true);
        let loud = crate::verbose::note_count();
        crate::note!("there are {} states in the intro", INTRO_FRAMES);
        assert_eq!(crate::verbose::note_count(), loud + 1, "`-v` did not bring it back");
        crate::verbose::set(false);
        assert!(!crate::verbose::on(), "and it can be turned off again");
    }

    /// `-v` changes nothing about *what* runs: like `--scale` or `--data-dir` it is a window-only
    /// option, so `antics -v` and `antics --verbose` are play commands, and neither is ever
    /// mistaken for a headless-output flag (which would make `--play -v` refuse to open a window).
    #[test]
    fn verbose_is_a_window_option_not_a_headless_one() {
        assert!(default_play(&args(&["-v"])), "`antics -v` plays");
        assert!(default_play(&args(&["--verbose"])), "`antics --verbose` plays");
        assert!(default_play(&args(&["--verbose", "--scale", "4", "--no-sound"])));
        assert_eq!(super::window_only_flag("-v"), Some(false));
        assert_eq!(super::window_only_flag("--verbose"), Some(false));
        assert!(plays_intro(true, "intro", &args(&["--verbose"])), "the intro still comes first");
        assert_eq!(first_headless_flag(&args(&["--verbose"])), None);
    }

    /// **The intro's pacing is the recording's, at the original's frame rate.**
    ///
    /// `recon/timeline/meta.json` holds 654 screens (index 0 the MAXX card) and the machine has
    /// 1,842 states, so one dispatch is `26 * 654 / 1842 = 9.23` ticks of the 240 Hz timer: the
    /// scene runs 70.8 s and the card's own 15 s wait comes first (`STAGE27A_SPEC.md` §5.1, §6).
    #[test]
    fn the_intro_is_the_recorded_654_frames_at_the_originals_rate() {
        assert_eq!(INTRO_FRAMES, 654, "meta.json's 654 captures, 0..653");
        assert_eq!(INTRO_CARD_HOLD_TICKS, 15 * crate::game::TICK_HZ as u64);
        let secs = INTRO_CARD_HOLD_TICKS as f64 / crate::game::TICK_HZ as f64;
        assert!((secs - 15.0).abs() < 1e-9, "the card holds {secs} s, not 15");

        let states = crate::intro::LAST_STATE + 1;
        let period = intro_dispatch_period(crate::game::FRAME_TICKS);
        assert_eq!(period, 9231, "26 * 654 * 1000 / 1842 = 9231 thousandths of a tick");
        let body_ticks = period * states as u64 / 1000;
        let body_s = body_ticks as f64 / crate::game::TICK_HZ as f64;
        // 654 frames of 26 ticks is 70.85 s, which is what the recording's screen count costs
        let want_s = 654.0 * crate::game::FRAME_TICKS as f64 / crate::game::TICK_HZ as f64;
        assert!((body_s - want_s).abs() < 0.1, "{body_s} s vs {want_s} s");
        assert!((body_s - 70.85).abs() < 0.1, "the arctic scene runs {body_s} s");
        // the whole thing, card included, stays under 90 s: a change that made it creep into
        // minutes would make an unskippable title sequence
        let total = secs + body_s;
        assert!(total < 90.0, "the intro is {total} s");

        // `--fps N` scales the dispatch period with the frame period, and a 0 is clamped to 1 tick
        for (ft, want) in [(26u32, 9231u64), (10, 3550), (6, 2130)] {
            assert_eq!(intro_dispatch_period(ft), want, "--fps -> {ft} ticks a frame");
        }
        assert_eq!(intro_dispatch_period(0), intro_dispatch_period(1), "0 ticks is a parse error");
    }

    // -----------------------------------------------------------------------------------------
    // v0.1.3 — no console box beside the game, and a message box for a fatal start-up error
    // -----------------------------------------------------------------------------------------
    //
    // The Windows half of this stage (`src/winui.rs`: `GetConsoleProcessList`, `FreeConsole`,
    // `MessageBoxW`) cannot be compiled or run here, so everything that *decides* what to do is a
    // pure function of three booleans and is tested below, on Linux. What is left in `winui.rs` is
    // the call itself.

    fn plan(detach: bool, boxed: bool) -> super::Plan {
        super::Plan { detach_before_window: detach, box_on_fatal: boxed }
    }

    /// **The decision itself.** A double-click or a Start Menu shortcut on a plain play detaches the
    /// console before the window opens and repeats a fatal error in a box; every other combination
    /// changes nothing.
    #[test]
    fn the_gui_launch_plan() {
        let plan_of = super::gui_launch_plan;
        // the case the installer creates: a shortcut, no arguments → a GUI launch, plain play
        assert_eq!(plan_of(true, false, true), plan(true, true), "a shortcut launch detaches + boxes");
        // `antics -v` from the same shortcut: the player asked for the console, so it stays, and
        // the error is already readable there — no box either
        assert_eq!(plan_of(true, true, true), plan(false, false), "--verbose keeps the console");
        // typed into cmd/PowerShell (the console is shared with the shell), or launched on Linux or
        // macOS at all: `is_gui` is false, so nothing happens
        assert_eq!(plan_of(false, false, true), plan(false, false), "a terminal launch is untouched");
        // a headless command line (`--shot`, `--install`, `--where`, `--scene round --ds`, ...):
        // its console is its output channel, whatever console it is
        assert_eq!(plan_of(true, false, false), plan(false, false), "headless never detaches");
        // and the two negatives together are still nothing
        assert_eq!(plan_of(false, true, false), plan(false, false));
    }

    /// **The classifier and the plan agree about what a shortcut means.** The installer's shortcuts
    /// run `antics.exe` with no arguments, so that command line — and only that shape of command
    /// line — must both "play" and plan the detach.
    #[test]
    fn a_shortcut_launch_is_a_plain_play_and_detaches() {
        for line in [&[][..], &["--play"][..], &["--scale", "4"][..], &["--no-sound"][..],
                     &["--data-dir", "/d"][..]] {
            let argv = args(line);
            let play = default_play(&argv);
            assert!(play, "{line:?} is a play");
            let p = super::gui_launch_plan(true, false, play);
            assert!(p.detach_before_window && p.box_on_fatal, "{line:?} from a shortcut");
        }
        // everything that names a headless mode (and `--install` / `--where`) keeps its console:
        // the file it writes, or the report it prints, *is* the reason it was run
        for line in [&["--shot", "f.bmp"][..], &["--frame", "350", "--shot", "f.bmp"][..],
                     &["--scene", "setup", "--shot", "f.bmp"][..], &["--install", "/game"][..],
                     &["--where"][..], &["--state", "s.json"][..], &["--demo"][..]] {
            let play = default_play(&args(line));
            assert!(!play, "{line:?} is not a play");
            let p = super::gui_launch_plan(true, false, play);
            assert!(!p.detach_before_window && !p.box_on_fatal, "{line:?} keeps its console");
        }
    }

    /// **Off Windows there is no GUI launch, whatever the console looks like.** `gui_launch` is the
    /// Windows-only half; this is the assertion that Linux and macOS runs of every previous command
    /// line are byte-identical to v0.1.2 (no detach, no box: nothing in this stage can fire).
    #[test]
    fn a_gui_launch_never_happens_off_windows() {
        if cfg!(windows) {
            return; // on Windows it depends on the console; the plan tests above cover the logic
        }
        assert!(!super::gui_launch(), "no console of our own off Windows");
        let p = super::gui_launch_plan(super::gui_launch(), false, true);
        assert_eq!(p, plan(false, false), "so every run off Windows is inert");
    }

    /// **The box is off unless the plan armed it.** The switch is a process-wide flag set once in
    /// `main` from `plan.box_on_fatal`; a test run starts with it off (that is what keeps the rest
    /// of the suite's stderr clean) and it reads back what was stored.
    #[test]
    fn the_fatal_box_is_off_by_default_and_follows_the_plan() {
        let was = super::fatal_boxes();
        assert!(!was, "a test process never arms the message box by itself");
        super::set_fatal_boxes(true);
        assert!(super::fatal_boxes());
        super::set_fatal_boxes(false);
        assert!(!super::fatal_boxes());
        super::set_fatal_boxes(was);
    }
}
