# Developer notes

These are the author's working notes for the port: what each subsystem does, the exact constants and
frame timings, and the evidence for every one of them. They are kept because they are the reason the
port behaves the way it does, not because a player needs them.

**Two things to know before reading:**

1. The evidence links (`recon/…`, `docs/adr/…`, `docs/projects/…`, `STAGE*_REPORT.md`) point into the
   author's **private** repository. They are provenance, not files you can open here. The private
   repo holds ~800 frames of recorded sessions captured from the original game — original-derived
   data, which is why it is not published. Nothing in this repository needs it; every test that
   consults those recordings **skips itself** when they are absent.
2. The port carries **no art of its own**. `antics --install <folder>` copies the nine files of
   your own copy of the game — `spy.exe`, `spytitl.exe`, `TITLE.DAT`, `TRAP.PIC`, `MAXX.PIC`,
   `DEMO.DAT`, `TEXT1..3.DAT` — into the per-user data directory, and the engine draws every screen
   from those nine files at run time (see [YOU-NEED-THE-ORIGINAL.md](YOU-NEED-THE-ORIGINAL.md)).
   Nothing is generated, extracted or converted at install time.

   The ten `assets/*.bin` files named in the stage reports below (`intro_*.bin`, `round_*.bin`,
   `setup_*.bin`) are the recon's **oracles**: captures the private repository keeps so a test can
   check what the engine derives. No engine code reads them (stage 29 removed every loader); a test
   that uses one reads it through `orig::oracle()` and skips when it is not there, which is every
   public checkout.

---

## Build and run

```bash
export PATH="$HOME/.cargo/bin:$PATH"      # distro cargo 1.93 is too old
cargo build --release --offline
cargo test --release --offline            # 394 tests: intro, selector, chrome, round, engine, terrain, items
ANTICS_HIDE_ORIGINAL=1 cargo test --release --offline   # the same suite as a checkout without the game
./target/release/antics --frame 400 --shot /tmp/f.bmp
```

| flag | meaning |
|---|---|
| `--frame N` | which state of the intro to render headless (0 = the MAXX card, 1…1841 the script machine's dispatches; clamped) |
| `--shot FILE` | write the frame as a 24-bit BMP at native 320x200 |
| `--state FILE` | write `{frame, scene, slots, strip, palette}` as JSON |
| `--data-dir DIR` | the directory holding the player's nine files (default: the per-user data directory `--install` wrote — see [YOU-NEED-THE-ORIGINAL.md](YOU-NEED-THE-ORIGINAL.md)); `--assets DIR` is the same thing under the harness's older name |
| `--scene setup` | render the setup selector instead of the intro |
| `--island 0-6` | iceberg shape, 7 variants |
| `--launch 0\|1` | launch site: 0 = HIDE sign, 1 = SHOW sign |
| `--players 0\|1` | 0 = one spy (the original's default), 1 = both spies |
| `--difficulty 0-4` | displayed as 1-5 |
| `--cursor 0/2/4/6/8` | selection ring: island / launch / players / difficulty / GO! |
| `--keys up,down,left,right,return` | replay key presses through the selector |
| `--upto N` | apply only the first N keys of `--keys` |
| `--trace FILE` | write one JSON state per applied key (the setup selector) |
| `--go` | act on GO!: commits the choices, reports `started: true` (the round is M3) |
| `--scene round` | render the round's room view from a recorded data-segment dump |
| `--ds FILE` | with `--scene round`: a 65536-byte data-segment dump (a recorded memory image, see below) |
| `--check` | with `--scene round`: report mismatching strip bytes vs the dump's own strip at `DS:0x1A92` |
| `--both` | with `--scene round`: compose player 2's view too (bottom console at y=118) |
| `--objects`, `--spies` | with `--scene round`: the two sprite layers — **on by default** since stage 3 (they close on 881 of 1082 dumps) |
| `--from-ds FILE` | with `--scene round`: seed the **engine** from a recorded dump (level table, both spies' state, clock) instead of starting a round |
| `--check-engine` | with `--from-ds`: diff the engine's own strip against the strip in that dump, per console |
| `--lift-blocks` | with `--from-ds`: copy the dump's object blocks in (verification only — the item/trap layer is built at round start and is not modelled) |
| `--play` | open the SDL2 window (3x integer scale); with `--scene setup` it starts on the selector, with `--scene round` in the round |
| `--script FILE` | with `--scene round` and no `--ds`: drive the **engine** headlessly, one tick per script line (a game frame is 26 ticks; `*N` repeats a line) |
| `--fps F` | sweep the frame period: the tick count is `round(240 / F)` (the original is 26 ticks = 9.2298 fps, the default); everything frame-driven follows it |
| `--shot-every N DIR` | with `--script`: write `DIR/frame_NNNNN.bmp` every N ticks (and at the end) |
| `--marker` / `--no-marker` | the placeholder spy marker (on by default; the real sprite does not close) |
| `--seed N` | the seed of the round-start PRNG (`FUN_26c0_20cd`): the same seed and island give the same object layout **and** the same two start cells. An explicit seed also **pins** the layout in `--play` (it turns `--obj-clock` off) |
| `--obj-clock` | seed the item placement from the DOS clock (`INT 21h AH=2Ch`, `CS:20cd`'s normal-play branch) instead of the attract round's fixed `DX = 0x770D`: every round gets its own layout. **On by default in `--play`** (each GO! and each restart takes a fresh reading); **off for every headless path**, so scripts, probes, soaks and the recordings' replays stay exact. `--seed N` or `--obj-state N` pins the layout again. `--random-start` is an alias (plus a clock-derived AI seed). See `recon/round/port_check/STAGE22C_REPORT.md` |
| `--layout` | with `--scene round`: print the round-start object layout as text (band, row, entry, world x, code, and whether the code is hidden under a mound) instead of only drawing it |
| `--dump-objects` | with `--script`/`--from-ds`: print every non-zero object byte of the five blocks at the end of the run (`band, row, entry, code`) plus the carried/kit/mask words — the before/after oracle for the item actions |
| `--no-items-ui` | do not draw the trapulator's dot strip over the lifted surround (it is drawn by default, and its cells are the plate's own pixels — see `STAGE7_REPORT.md`) |
| `--obj-state N` | force the placement PRNG's 16-bit state (decimal or `0x` hex). `0x469` is the attract-mode seed the original writes at `DS:0x31B0`; `7` reproduces the recorded attract round's layout byte for byte (see `recon/round/port_check/STAGE6_REPORT.md`) |
| `--start-screen N`, `--start-x N`, `--start-x2 N`, `--start-y N` | force the starting room/position (P1, P2, both) instead of drawing it |
| `--void-solid` / `--void-walkable` | treat the level-8 "ocean"/void band (mask code 1, a *climb*) as a wall — **off by default**, because the original walks on it; on, a spy cannot leave the room's own cells |
| `--igloo-entry walk\|key\|off` | how the igloo door opens. `walk` (**default**) is the original's own rule: the mask's code-7 cell is the door and walking onto it enters (`FUN_26c0_1e0f`, `AX == 7`). `key` is stage 4's action-key version (fires one frame earlier, at the alcove); `off` leaves the cell walkable and inert, which is what stage 4 shipped |
| `--visitor-draw` / `--no-visitor-draw` | draw the visitor's sprite into the host's console (`FUN_26c0_0c93`'s two-spy loop, y-sorted). **On by default**; off reproduces stage 4, where the host saw an empty room |
| `--visitor-eval-mover-only` / `--visitor-eval-both` | re-test the shared-screen visitor flag only in the moving spy's frame (stage 4) instead of every frame for both spies. **Both is the default** and is what the recordings show (21 recorded clears happen with the visitor standing still), so a host walking away un-blanks the other console |
| `--spy-block on\|off` | the two spies' **bodies** block each other (`CS:097c` → `FUN_26c0_35e5`: same room, one of them a visitor, `\|Δy\| <= 1` and `\|Δx\| <= 3` against the tentative step → the step is refused). **On by default**; `off` is the stage-8 behaviour, where they walk straight through each other, and exists so the defect and the fix can be measured on one recording |
| `--strip-out FILE` | with `--from-ds`: write the engine's two 44x58 console strips as raw bytes (player 1 then player 2) — what the frame-comparison scripts diff against `DS:0x1A92` |
| `--tick-trace FILE` | with `--script`: one CSV line per tick (`tick, p1_screen, p1_x, p1_y, p1_map_x, p1_dir, p1_frame, p1_step, p1_blank, <the same for p2>`) |
| `--clock N` | override the island's round clock (seconds); default is the island's own |
| `--carry CODE`, `--carry2 CODE` | **demo aid**: put an item code 1..12 straight into that spy's hand, as a trapulator take would (the panel is not driven); `--carry-at TICK:CODE` / `--carry2-at TICK:CODE` do the same at a given tick, so a panel can plant a stick with one hand and detonate it with the other |
| `--heat N` | **demo aid**: seed both spies' body-heat word (3 full .. 63 frozen), so a panel can show the bar at a chosen fill or the freeze without waiting 138 frames a cold step |
| `--heat2 N` | **demo aid**: seed only the second spy's heat, so a panel can freeze *one* spy while the other plays |
| `--parts` | **demo aid**: `kit |= 0x2C0` on both spies — all three parts are in the briefcase, so a spy who steps on the hatch launches instead of meeting the bear (`--start-hatch` puts him on it) |
| `--scale N` | window scale, 1-8 (default 3 = 960x600) |
| `--verbose`, `-v` | print the diagnostic chatter (off by default, see below) |

`ANTICS_ANIMATE=0` disables the animated band, to compare against the static scene.

### `--verbose`: what the program says, and when

Since v0.1.2 the program is **quiet by default**: a plain `antics` prints nothing on stdout or
stderr while it plays. Everything it used to say about its own progress is a **note**, and notes
print only with `--verbose` / `-v` (anywhere on the command line; it takes no value and is a
window/data option, so `antics -v` plays).

One switch, one macro: `static VERBOSE: AtomicBool` in `src/verbose.rs`, set once in `main` from
`verbosity_requested(&argv)`, read by `note!` (a `println!` behind `verbose::on()`). Use `note!` for
new informational lines — never `println!`/`eprintln!` — and leave these alone:

* **errors** — every failure and every usage/argument error, on stderr, with its exit code;
* **explicitly-requested output** — `--install` and `--where`, and every flag whose *purpose* is to
  print (`--state`, `--shot`'s report, `--layout`, `--dump-objects`, `--tick-trace`, `--state-every`,
  `--sound-dump`, `--cpu-probe`, `--cpu-corpus`, `--trace`, the `--scene round --ds` report, ...).
  A flag that exists to print something prints without `--verbose`.

The notes today: the intro's timing and skip, the item placement's clock seeding, the window
geometry, the speaker (or `--no-sound`), the selector's key legend, `GO!`/restart/abandon, every
item change and room change, the round's end, the attract demo, the panel's idle timeout and the
`F2` save. Tests that `eprintln!` their own progress are not affected (they are test output, not
program output).

The CLI is the contract in `docs/adr/0002-port-cli-contract.md`. Rendering is a pure function of
(the player's nine files, frame index): no clock, no RNG, no threads.

### The command in a message is the one that works

A binary in the folder a player unpacked it in is **not on `PATH`** on Linux or macOS, so
`antics --install ...` is `command not found` there. Every message that names a command prints it
through `install::command()` — `./antics` on unix, `antics.exe` on Windows — never the bare name
and never a guess at the player's `PATH`. `install::tests::the_message_prints_the_command_that_works_on_this_platform`
and `the_not_found_message_tells_the_player_what_to_do` hold that.

## The player's nine files (the install directory)

`antics --install <folder>` **copies** these nine files into the per-user data directory, after
checking each one's size and SHA-256; `src/install.rs` holds the one identity table and
`install::tests::the_docs_list_every_identity_row` keeps the table in
[YOU-NEED-THE-ORIGINAL.md](YOU-NEED-THE-ORIGINAL.md) equal to it. Nothing is generated.

| file | what the engine reads out of it |
|---|---|
| `spy.exe` | the round, the selector and the console chrome: the level/trap tables in its 64 KB data segment, the draw lists and records in its code, the DEMO.DAT address table |
| `spytitl.exe` | the intro: the script machine's code, its 13 static frame records, the two mask planes and the scripts |
| `TITLE.DAT` | the intro's scene picture (`DS:0xC42`'s background) |
| `TRAP.PIC` | the round's console surround and the selector's screen template (`CS:0x29F3` reads it straight into `B800:0000`) |
| `MAXX.PIC` | the intro's frame 0, the MAXX OUT! card |
| `DEMO.DAT` | the attract demo's recorded key masks, one word a frame |
| `TEXT1..3.DAT` | the intro's three caption strips (54 x 80, 2 bpp) |

The formats of those files are the game's own, not the port's; nothing in this port writes a data
file. The `assets/*.bin` files the stage reports mention are the recon's oracles and are never read
by the engine.

## How a frame is composed

1. Palette 0 means the MAXX OUT! card: frame 0 is `MAXX.PIC`, pushed as it is and stopped on.
2. Otherwise the scene starts from `TITLE.DAT` (`DS:0xC42`) and the pieces are stamped by the
   original's own blitter (`CS:0x0850` and the 12-byte descriptors the script builds).
3. Pieces passing behind an iceberg are clipped by the two mask planes in `spytitl.exe`
   (`DS:0xB84`), not by a measured mask.
4. The caption strip for the current state is composited from `TEXT1..3.DAT`.
5. `ANTICS_ANIMATE=0` freezes the animated band for a static comparison.

The `intro_*.bin` oracles decode exactly this composition, which is how it is checked
(`intro::tests`, and §4 of `STAGE28A_REPORT.md`).

## The setup selector (M2)

The frame is **drawn** by a port of the original's own draw list (`src/selector_draw.rs`,
`CS:0x45B0` and the records it reads out of `spy.exe`) from `spy.exe` + `TRAP.PIC` alone. This
module used to be three captured files layered on a captured background; the drawn frame equals
that capture on all 64,000 pixels, and the three oracles (`setup_bg.bin`, `setup_elements.bin`,
`setup_rings.bin`) now only check it.

```
screen = TRAP.PIC template + the island bitmap + the HIDE/SHOW sign + the pair + the
         difficulty bar and its digit + GO! + the cursor ring for the zone
```

Each element is a **masked or opaque record** in the data segment, stamped at the byte column and
bottom row its pointer table names (`DS:0x31BB` island, `DS:0x31D7` launch, `DS:0x31DB` players,
`DS:0x31E1` difficulty, `DS:0x7806` GO!, `DS:0x31EF`/`0x31F9` the ring's height/width).

Option semantics are the game's own and were confirmed against its live memory, not inferred:
island 7 shapes; launch site a HIDE/SHOW toggle; players a flag where raw `1` means *one* spy;
difficulty five levels displayed 1-5.

## The three ports of the original's own drawing code (stages 28a-28c)

As of stage 29 these are the *only* paths: the captured art they replaced is kept in the private
repository as an oracle, and no engine code reads it.

**`src/intro.rs` — `spytitl.exe`'s script machine.** The original's intro is not a recorded
animation: a byte-code script drives ten 12-byte sprite descriptors through a masked blitter and a
54-row band blitter, and the port executes that script. It reads `spytitl.exe`'s own code, its 13
static frame records, the two 2 bpp mask planes (`DS:0xB84`), the scripts and the quit word
(`DS:0x9CC8`), plus `TITLE.DAT` (the scene), `MAXX.PIC` (frame 0) and `TEXT1..3.DAT` (the caption
bands). Verified against `recon/timeline/frames.bin` — 588 recorded screens at the recorder's
lag-adjusted dispatch — where **505 of 588 frames are byte-identical, median 0 differing bytes**,
and against the four `intro_*.bin` oracles for the static tables (the card is `MAXX.PIC` pixel for
pixel). Residual and its cause: `STAGE28A_REPORT.md` §4.

**`src/selector_draw.rs` — `spy.exe`'s selector draw list.** `CS:0x45B0` blits two 44-byte x 58-row
strip buffers into the CGA interlace; the port reproduces the strips and blits them de-interlaced.
Every element is a record stamped at the pointer table's column and bottom row, and the strips' own
plate is a uniform `0x55` fill around the island. Verified against all 21 recorded selector states
(`recon/setup/`, 0 mismatched pixels), against the three `setup_*.bin` oracles (28 element states,
5 ring zones), and by mutating one byte of `spy.exe` at a time and watching exactly the pixels it
feeds move (`selector_draw::tests`).

**`src/chrome.rs` — `spy.exe`'s console chrome.** The round's surround is `TRAP.PIC` decoded and
pushed (`CS:0x29F3`), with both viewport rects filled flat cyan; over it the four widgets are drawn
from the data segment's own records, each a port of one routine: the body-heat gauges
(`CS:0x21BD`, two 14-byte row records at `DS:0x15EE`/`0x15FC` through the `DS:0x160A` template), the
clock's four opaque 8 x 2 glyph records (`CS:0x229E`, `DS:0x7004` + 18 a digit), the tray dots
(`CS:0x3278`, seven 4-row records held/absent at `DS:0x15BC`/`0x15CA`) and the kit box
(`CS:0x31C8`, three masked records stamped into the `FF` canvas at `DS:0x1494`). Verified against
`assets/round_bg.bin` (63,198 of 64,000 pixels identical; the 802 differences are exactly those
four widgets), against `round_kitbox.bin` (3 of 3 icons byte-identical) and `round_digits.bin`
(7 of 7 rows of all ten digits), and against whole recorded frames (`frame_check3.py`: surround
median 0 px, and every residual cluster inside a widget rectangle).

## The round view (M3 stage 1)

```bash
./target/release/antics --scene round \
  --ds recon/round/sessions/playtest1/ds_00521.bin --check --both
./target/release/antics --scene round \
  --ds recon/round/sessions/rocket-room-2/ds_00060.bin --shot /tmp/room.bmp --both
```

The room viewport is a pure function of `spy.exe` and one 64 KB data-segment dump: no captures, no
baked pixels. (Its *surround* was the setup screen until stage 29 — a stage-1 shortcut — and is now
the round's own plate, `decode(TRAP.PIC)`; on `playtest1/ds_00521.bin` that moves 1,250 of 64,000
pixels, all of them outside the two viewports.) The plate bank is read from the art segment at **file 0x12160** (segment 0x11F6 is a
relocated immediate, so it is 0x21F6 at run time), the tables (`0x24BE` levels, `0x25AE` gates,
`0x24AA` row bases, `0x25C6` plates) come out of the dump, and `compose_passes` replays the
blitter's up-to-three passes of `min(26-CX, 44-DX)` bytes per row.

Numbers on 1082 recorded dumps from 6 sessions (832 of them already drawn): **301 real rooms
reproduce all 2552 strip bytes exactly** outside the box where the spy sprite sits, plus 84
blanked-console frames (the easy, flat case, counted separately). The residual on a real room is the
spy sprite alone, median 95 bytes of 2552. Object and spy layers are implemented from the machine
code but do not close, so they are off by default. Details, per-session tables, failure taxonomy and
proof images: `recon/round/port_check/STAGE1_REPORT.md`.

## The playable round (M3 stage 2b)

```bash
# the real thing: setup selector -> GO! -> walk island 1, in a 3x-window
./target/release/antics --scene setup --play

# straight into a round, with the spy on the door column
./target/release/antics --scene round --play --island 0 --start-x 141

# headless: replay a key script, one line = one 240 Hz tick (a spy step is 26 ticks), `*N` repeats
./target/release/antics --scene round --island 0   --script recon/round/port_check/play/walk_island1.txt   --shot-every 30 /tmp/frames --state /tmp/round.json
```

Controls: **P1** arrows + RETURN (fire); **P2** A/W/D/X (`X` is down) + Q. `Esc` leaves the round,
`R` restarts it, `F1` toggles the spy marker, `F2` writes a BMP.

**The fire key, and what it does** (stage 7 — `STAGE7_REPORT.md`):

| gesture | effect | where in the original |
|---|---|---|
| tap fire standing on an item, or one step in front of one | **pick up**. A `0x8x` byte is a mound: its low five bits are the item, so this is also how you **dig** | `FUN_26c0_2d22` -> `FUN_26c0_373b` |
| tap fire again while carrying | put it away: a tool goes into its trapulator slot, anything else is **dropped** where you stand (the bare code) | `FUN_26c0_315f` / `FUN_26c0_38e1` |
| hold fire, then press a direction | **bury** the carried item (`code` OR `0x80` — a mound), or with a tool in hand that tool's own action | `FUN_26c0_2d7e` -> `FUN_26c0_2de3` |
| tap fire twice with nothing to pick up | open the **trapulator**; north/south pick a slot, fire takes it, east/west closes it | `CS:2d50`, `FUN_26c0_2a4e` |
| hold fire + a direction with an empty hand, both spies on one screen | **throw a snowball**: eight drawn frames, then a flat flight at three world bytes a frame that hits or dies at the window's edge (`STAGE16B_REPORT.md`) | `FUN_26c0_2f87` |
| fire with the **briefcase** in hand over a rocket part | put the part *into* the case (the deposited mask bit sets); over anything else, the case is swapped for it | `CS:3822` |

The original has **two** joystick buttons — A (`0x327F`) is the tap, B (`0x3283`) is the
direction action — and the port maps both onto its one fire key: a press with no direction is A, a
direction pressed while fire is held is B. The one behavioural cost is that "put away" happens when
fire is *released* rather than on the next frame, because otherwise holding fire to add a direction
would drop the item first (`STAGE7_REPORT.md` §2.3).

`game.rs` is the engine: a fixed **240 Hz tick**, no wall clock, no RNG outside the round-start draw.

The tick is not a convenience rate: it is the original's own timer. `CS:4574`/`CS:4578` program PIT
channel 0 to divisor `0x136C` (4972) — 1,193,181.8 / 4972 = **239.98 Hz** — and `CS:4562` points
INT 1C at `CS:4196`, which increments `DS:0x33F4` on every tick. The frame-sync loop (`CS:063c`) waits
for that word to reach `0x1A` (26) with the word zeroed at the top of each frame (`CS:0658`), so a
game frame is **26 ticks = 108.34 ms** and the spies move **9.23 frames a second**, not 10
(`STAGE16A_REPORT.md`; the old "10 Hz / 100 ms" reading was 6 of the port's own 60 Hz ticks).

The round clock counts **9 frames a second**, so a displayed second is 975 ms: `CS:0680` compares the
clock's word `DS:0x33F6` with `0xDC` (220) and clears it when it fires, and that test runs **once per
frame** — 8 frames (208 ticks) never reach it, 9 (234) always do. A 360-count island round is
therefore 3,240 frames = **351 s** of wall clock, and the original's clock runs 2.5 % fast.

A script line is one **tick**, so one frame of input is 26 lines and a one-second script is 240
lines; `--fps F` sweeps the period (`--fps 10` gives back the old 100 ms frame for a side-by-side).
Everything in it is in the original's own units — `x` is a byte column of the 312-byte world row
(26 bytes per cell, 44 bytes in the viewport), `y` is the foot row of the 58-row viewport (34..58),
the screen id is `DS:0x329F` (even, bigger = further up). The per-island round-start data comes
straight out of `spy.exe`: the level table, the clock (360/480/660/840/1080/1320/1620 s for islands
1-7) and the start screens are eight 7-entry word arrays at `DS:0x72C0`.

Island 1 is two rooms — screen 4 and screen 6 — joined by one door pair at `x = 26*5 + 11 = 141`, and
the scripted walk in `recon/round/port_check/play/` goes through it and back
(`recon/round/port_check/play/walk_strip.png`, `window6-room-changed.png`).

## The round's frame (M3 stage 3)

The engine draws the same room the dump path draws: `--from-ds` seeds it from a recorded dump and
`--check-engine` diffs its composition against the one the game left in that dump — **0 of 2552
bytes on 881 of 1082 dumps**, identical to the dump path on all 1082. Against a recorded CGA frame
the whole 320x200 screen differs by a median of **162 of 64000 pixels** (best 50), with **0** pixels
of difference in either viewport outside the spy's own footprint on 55 of 69 settled pairs.

The frame is the console's **own** pixels, from the player's files: the surround is `TRAP.PIC`
decoded and pushed, and the four widgets (body-heat gauges, kit box, clock digits, tray dots) are
drawn by `src/chrome.rs` from the records in `spy.exe`'s data segment — a port of the four routines
that draw them (`CS:0x21BD`/`0x229E`/`0x3278`/`0x31C8`). The old captured plate and the captured
glyph/icon files are oracles only; `assets/round_bg.bin` differs from `decode(TRAP.PIC)` in exactly
the 802 pixels those widgets own.

```bash
./target/release/antics --scene round --from-ds recon/round/sessions/playtest2/ds_00210.bin \
  --check-engine --lift-blocks
tools/venv/bin/python recon/round/port_check/frame_check3.py --limit 15
```

The **body-heat bars** are drawn since stage 14 (one row per cold unit, from `DS:0x2F58`/`0x2F5A`),
so the frame residual is now the sprite layer alone. The items **are** placed since stage 6 and now
move (stage 7); the original listing said a live round started with empty blocks, which stage 6
fixed. The spy sprite, the walk cycle (three legs
phases, `DS:0x2EFC` bounces 0/2/4/2), the clock digits and the console surround are the game's own.

## The round's rules (M3 stage 4)

The collision the earlier stages could not find is the per-level **behaviour mask**
(`FUN_26c0_366e` + `FUN_26c0_1e0f`, `src/terrain.rs`): 26 bytes x 13 rows per level, one byte per
world unit and two-row band, with `5`/`6` solid. It is what stops a spy at a wall, what makes an
igloo block, and what the round-start placement tests (`code == 0`) before it accepts a cell. The
same pass added the bounce walk cycle, the standing frame, the 10 Hz frame and the shared-screen
**visitor window** (`map_x - 4 < x < map_x + 0x29`). Numbers and evidence levels:
`recon/round/port_check/STAGE4_REPORT.md`; the demos are
`recon/round/port_check/play/stage4_play.py`. 

## Stage 8b: six fixes from the owner's hand test

`recon/round/port_check/STAGE8B_REPORT.md` (numbers, evidence levels and the reproduce lines).

| defect | cause (evidence level) | fix |
|---|---|---|
| a tap on the trapulator's left/right is sometimes lost | the port sampled the keys once per 100 ms game frame; the original's `DS:0x3287`/`0x328F`… are **latched keyboard flags** (read) | every key seen down during a frame is latched (`Game::pending`), and a left/right press stays latched until the trapulator serves it (`SpyState::cursor_req`) |
| the selection mark flashes "a second light up and to the right" | `FUN_26c0_45b0`'s sprite header is (rows, byte width); the port read it as (width, rows), so every mark was transposed (read + measured on 91 recorded panel frames) | `items::sprite_at` fixed; the mark is now the slot's own dot cell, 4 rows tall, 8 px wide (4 for the pickaxe and the dynamite) |
| a visitor who leaves a shared room through a door gets a console showing the wrong column, without him | the port ran the `CS:0876` branch (the *other* spy inherits the leaver's scroll) for both cases; `CS:0887` gives a *visitor* leaver his own `map = x - 0x16` (read + measured: 4 of 5 recorded visitor door-steps land at `x-map = 22`) | `Game::after_room_step` branches on who was the visitor |
| two spies on one screen walking the same way make the view jump at twice speed | the visitor's scroll guards were inverted (`CS:00cf..00df` rejects when `map+0x22 <= x`); a visitor past the host's edge scrolled the host's view *and* the host's own frame did, so one `map_x` gained 4 units a frame (read + measured: the recording keeps it flat) | both guards in `Game::scroll` corrected |
| the "blue package" in hand | **not a port fault**: code 11's in-hand sprite is pose set `0x5A`, which is 13 white + 1 magenta pixels (no cyan) both in the executable's art and in the recordings; the port reproduces 254 of 321 recorded carried-code-11 consoles at **0 differing pixels** | none — reported with the numbers, open for the owner |

## Stage 9b: what happens when the two spies meet

`recon/round/port_check/STAGE9B_REPORT.md`. The owner's defect 8 was *"the spies walk straight
through each other"*. What the executable does at `CS:097c` — between the terrain switch and the
hatch's touch test, on every step either spy takes — is **`FUN_26c0_35e5`, and it is a blocking
collision and nothing else**:

```text
same room?  and  (this.0x2F7E or other.0x2F80, a visitor)  and  this spy not mid-animation
    and  |cand_y - other.y| <= 1   (DS:0x2F1C = 1)
    and  |cand_x - other.x| <= 3   (DS:0x2F20 = 6, narrowed by the +3/-3 at CS:3648/365b)
-> FUN_26c0_3396   ; zero the velocities: the same "refuse the step" a wall uses
   0x2FA4 = 1      ; "my last step was blocked" — read back only by the attract-mode autopilot
```

No state word is written to either spy, nothing leaves either hand, and no life, score or heat word
is touched: **there is no fist-fight, no stun, no knock-out and no item drop in a meeting** in this
build. The knock-out exists but belongs to the snowball (`CS:30c9` writes `0x32CF = -1` on the spy
it hits) and to the traps, which are still not modelled. What a player feels is a wall: the spy stops
on the standing frame and the key does nothing until you step off the row (`|Δy| <= 1` is one row,
and `y` steps by 2).

The port runs that test in `Game::spy_contact`; `--spy-block off` restores the walk-through for
comparison. Measured against the recordings: no recorded pair on one row is ever closer than **4**
units (199 same-row dumps in which a visitor is present, 0 below 4 — 20.8 expected from the shape of
the one-row-apart distribution), while one row apart they reach `dx = 0` (32 dumps at `dx <= 3`).

## Stage 10: the hatch sequence, the shared-screen push, the speed while scrolling

`recon/round/port_check/STAGE10_REPORT.md` (the readings, the corpus tables and the per-frame
replays). The owner's three observations, and what the executable does:

| observation | cause (evidence level) | fix |
|---|---|---|
| the thrown spy "teleports left, landing a little north"; the legs vanish; a spy with the briefcase is left stuck after the first frame | the animation's end test was `2(f+1) > ceiling` instead of the code's `2f > ceiling` (`CS:0996`, `0x2EFC = 2f - 2`), so **the last script frame of every state never ran** — for the throw that entry is `vy = +2, vx = 0` (read + measured: the recording lands at 123,52, the old port at 123,50) | `Game::hatch_advance` uses the code's test; the descent/thrown/climb-out trace now matches `playtest4` frame for frame |
| the other spy "falls on his bum" instead of laughing | `CS:2915` starts state `0x0C` on him through `277c`, whose `CS:27e9` path switches his pose set to `word[0x2CC4 + 0x0C]` = **`0xDC`** (read + measured: ds_01783 is `0xDC`, back to `0x24` ten frames later); stage 9a kept `0x24` | `round::anim_set` + the save/restore in `hatch_end_state`; `laugh.png` puts the recorded and the port's frames side by side |
| the landing "legs do the walk animation" / the descent's legs do not move | the legs index is `+2` for raised states **except `0x1C`**, which reads `word[0x2D22 + 0x2EFC]` = `0,2,4,2,0,2,4,2,0` (read: `CS:e2d`), while the body never advances (read: `CS:d03`) | `PoseParams::records`; the climb-out now cycles three leg records with a static body |
| the descending spy slides instead of standing still, and the hatch's clip is not applied | the blitter's `DS:0x31B2` is a **row shift that also drops the bottom rows** (`CS:3461..34ca`), and the body's own `sub $9` (`CS:d2e`) is skipped only on the laugh's branch | `round::masked_blit_clipped`, `PoseParams::body_place`/`legs_place`/`legs_drawn` |
| a spy with all three parts walks in and is left frozen, half-sunk | the launch branch (`CS:2865`) is a state, not a hang: `0x2F70 = 1` (the rocket is drawn instead of him), `0x2F64(other) = 2`, `0x2F7E(other) = 1` (read + measured: ds_01814..01816). The ascent itself is still not ported | `Game::launch`; `--state` reports `rocket` |
| a visitor can push the host's frame east to the cut-off but only halfway west | `CS:010d..011d` refuses a west push when `host.map + 0x1C < host.x`; stage 8b wrote that constant as `SCROLL_RIGHT_EDGE - 0x10` = **14** instead of **28** (read + measured: the recorded host stops at screen **30** west and **6** east, 51 west and 57 east recorded pushes) | `VISITOR_PUSH_WEST_MAX` etc. in `Game::scroll` |
| the front spy moves at "twice the pixels per step" | **not reproduced as a port defect**: the recording moves a spy 2 units a frame while the view scrolls and 1 while it does not (2,579 of 2,648 map-changing 100 ms pairs are exactly `dx = 2, dmap = 2`; 2,752 of 2,796 moving flat-view pairs are `dx = ±1`), and the port now gives the same two numbers. What *was* wrong was the shape of the push (the row above) and the movement loop's **iteration cap**: the frame re-enters the movement as long as the view scrolls (`CS:0a44..0a49`), which the port stopped after two of (a read-justified fix; at 100 ms resolution the corpus never shows a step over 2 units). Both fixed | the demo, the numbers and the honest remainder: `STAGE10_REPORT.md` §3 |

## Stage 11: four defects from the owner's hand test

`recon/round/port_check/STAGE11_REPORT.md` (the disassembly, the corpus tables and the failing-first
numbers).

| observation | cause (evidence level) | fix |
|---|---|---|
| "the black spy is not triggering the polar bear, he just walks over the hatch. White spy does trigger it" | the trigger gated on the **visitor flag** (`0x2F7E`). `FUN_26c0_2392` has no such test — `CS:097f` calls it for both spies and it reads the feet cell and the other spy's animation state only; the `if (0x2f7e(si))` the port was reading is `CS:0be7`, the **object layer's blitter** (read). The visitor is normally the spy who walks into the other's window, i.e. usually the black spy (measured: black is the visitor in every shared-window frame of the live scenario) | `Game::hatch_touch`; the live two-player scenario goes `idle` × 101 frames → `descend 16 / bear 17 / close 10`, landing (97,52), white laughing `0xDC` |
| "parts no longer disappear from the other spy's kit … both spies can show the same parts at once" | `CS:3896..0x38be`: taking the **briefcase** stamps the per-spy ledger `DS:0x322D` with the holder's `SI` for every *deposited* part; the port only did that on a deposit and on a ground pickup (read + measured: `playtest5` ds_00897→00898 moves two ledger words from black to white on the pickup frame; 155/155 recorded hand transitions agree with the five rules) | `Game::pickup`'s `take >= 12` branch |
| "it may be that the spy NOT pushing the screen edge slows to half speed … in the port one falls off the screen" | **not reproduced**: the port's per-frame numbers equal the recording's in every class the corpus holds, including the one that separates the pair — when only one spy pushes, the other's `x` does not move with the view (91 + 91 recorded same-screen pairs are `dx 2/0, dmap 2/0`), and no instruction in `FUN_26c0_0000` carries it. The owner's exact case (both walking, one a visitor, a scroll) is **not in the corpus** — 0 of 1,545 one-frame pairs | the numbers and the honest remainder: `STAGE11_REPORT.md` §3 |
| on **SHOW** the hatch "only appeared once parts had been found" | the setup screen's launch zone never reached the round: `play_app`'s GO! copied the island and the player count from the selector and left `o.launch` at the command-line default (`false` = **HIDE**), so a SHOW round ran on HIDE and could only be revealed by `0x2EB8 == 0x6C0` (read). The restart key had the same leak | `opts_for_selector` + `started_opts`; islands 0..3 draw the hatch on frame 0 (**28 px** of hatch ink per console) and a pickup/drop/deposit leaves it at 28 px |

## Stage 12: the walk in step, and the briefcase under the thrown spy

`recon/round/port_check/STAGE12_REPORT.md` (the disassembly, the 25 replayed intervals and the
failing-first numbers). Measured against the owner's recording of the original,
`recon/round/sessions/orig-compare-1` — the datum stage 11 was missing.

| observation | cause (evidence level) | fix |
|---|---|---|
| "when two spies share a screen and walk the same direction close together, in the port one falls off the screen within a few seconds — the one not pushing the view edge moves at half speed" | **found.** `CS:0743..07b1`, the tail of the per-player move loop: on a frame where the mover's own move **scrolled** his view (`DS:0x31A6`) and the other spy holds the same horizontal key, on one shared screen and with the other not on the fire key / not raised / not out of the round, the mover adds **±1** to the other spy's `x`, terrain-probed with `FUN_26c0_366e`/`1e0f` and undone when the cell refuses (`0x31AA`) (read). The recording: 25 same-direction intervals, **separation constant in 25/25**, `dx −2/−2`, `−1/−1`, `2/2`, `1/1` per frame | `Game::push_in_step`, called from `step_frame` on a scrolled frame. Replay of every interval through the port: `x1` 180/235 → **234/237** exact, `x2` 136/235 → **234/237**, diverging separations **16/25 → 0/25** |
| "when the walking animation of the polar bear throw starts, the briefcase appears by the spy's feet for a frame or two" (the original does not show it — owner-confirmed) | the state machine starts a state with `0x2EFC = -2` and the original advances it (`LAB_26c0_0a06`) **before** drawing; the port drew the seed, and `-2` makes the climb-out's legs read `word[0x2D22 − 2] = 0x6C` — the **carrying** pose block — instead of `word[0x2D22] = 0` (read + unit test: legs record 43,800 → 33,568) | `PoseParams::records` feeds the tables the advanced frame (`SEED_FRAME` → 0), which also covers the laugh's `d44` body branch. **Not pixel-confirmed**: my reproduction renders identically before and after — see the report's §2.5 |

## Stage 13a: the saw, the water bucket and the ice pick — and the hazards they leave

`recon/round/port_check/STAGE13A_REPORT.md` (the branch-by-branch disassembly, the corpus numbers
and the demos). Fire held plus a direction now uses the tool in hand, and the trap it leaves bites
**both** spies.

| observation | what the original does (evidence level) | port |
|---|---|---|
| the ice bucket "can be taken in and out of the Trapulator but its action does nothing" | `FUN_26c0_2de3` `CS:2ec7`: with the spy's **own** bucket code (`0x2DB6+si` = 4 white / 5 black) the action takes the cell **one entry in front** (`0x32D7 ± 1`, the sign from `0x2F14`), refuses it unless it is empty and the terrain under the *spy* is 0/5/0x10/0x11, starts state 8 for **twelve frames** and writes `0x0D` at the end (`CS:28fa`) (read + 7 recorded spills) | `directed_action` + `tool_target` + the `Anim` state machine |
| — | the saw is the same six steps with state 4 and writes `0x0E` (`CS:28e9`): 10 recorded holes, every one one entry in front of the cutter | same |
| — | the ice pick needs the **door arch** under the spy (`0x366e` = 0x0A), then writes three `0x14` bytes at `block + (x>>1) - 1` with **no row term** — always the room's front row — and sets the `0x2F6C` latch so the icicle cannot hit its own placer (read + the `iceaxe` recording: island 3, screen 6, x 36, y 36 → entries 17..19 of row 0) | `ice_pick` |
| — | a climb under the spy (`0x2FAA` = 1 or 0x17, `FUN_26c0_366e`'s last result) refuses **every** directed action, the bury included | the head of `directed_action` |
| walking onto a spill/hole/icicle did nothing | `FUN_26c0_2444`, once per frame before the move: `0x0D` → state 0x0E (slip, 40 frames) and clear the byte; `0x0E` → state 0x10 (fall, 38 frames) and clear it; `0x14` → clear **three** bytes from `feet-1`, state 0x1A (36 frames), `y = 0x26`, `0x2F44` on the host. The slip and the fall call `FUN_26c0_315f` (put away what is held); the knock-out does not. **No ownership test** — the placer trips on his own trap too; the only immunity is `0x2F6C` (read + 9 recorded bites replayed byte for byte) | `hazard_check`, `Anim`, `spies[i].icicle_latch` / `icicle_hit` |
| the hazards' sprites | `0x0D` and `0x0E` are ordinary object codes (`DS:0x2D38[13]`, `[14]`), so the object layer draws them; `0x14`'s record pointer is `0x0000` and the layer's signed `CMP AL,0x14; JGE` skips it — the icicle is invisible (read; 246 of 279 recorded frames that hold a hazard on screen are pixel-identical in both viewports) | nothing to draw; the existing object path does it |

**What is deliberately not the original's**: the hazard check sits at the *top* of the frame body
rather than after the key handling, so a hazard preempts that frame's press instead of following it
(100 ms, no recorded case separates the two); and the animation branch does not call
`FUN_26c0_35e5`/`0x2392`, so a spy cannot be *shoved* and cannot open the hatch while an animation
owns him. `--carry CODE` is a demo aid (a trapulator take without the panel).

## Stage 13b: the dynamite stick and the plunger

`recon/round/port_check/STAGE13B_REPORT.md` (the disassembly, the 23 plants / 3 detonations /
3 duds, and the demos). The stick is planted by the bury the fire key already had; the detonator is
`FUN_26c0_2de3`'s first branch.

| observation | what the original does (evidence level) | port |
|---|---|---|
| a stick planted with the bury gesture | `FUN_26c0_38e1`'s bury branch ORs the planter's **owner tag** (`0x3307` = 0x20 white / 0x40 black) and bit 7 into the byte: `0xA8` white, `0xC8` black. Over an occupied cell it ORs onto the byte already there — so a stick set on a part keeps the part's code in its low five bits (`0xCA` = part 10 + black's tag) — but a second *stick* on the same cell is refused and steps west (`CS:3930`'s `and $0x60`). 23 recorded plants, every one at the planter's own feet (read + measured) | `drop_item`'s dynamite branch, `items::player_tag`, `items::OWNER_TAGS` |
| the detonator "does nothing" | `CS:2dfe..2e61`: the **opponent's** cached cell (`0x32D3`/`0x32D7`), tested against **my** tag. Hit: clear the byte when `(byte & 0x1F) == 8`, state 0x12 (pose 0x1B4, **36 frames**) on the victim, `315f` on the victim (he puts away what he holds), then the firer's own state 0x0A (pose 0xD4, 4 frames) — which plays on a **dud** too. The plunger is not consumed, and the firer gets no laugh (read + 3 recorded detonations replayed byte for byte) | `detonate`, `blast_on_tap` |
| walking onto a stick | `FUN_26c0_2444` returns for every `byte & 0x1F <= 0xC`, so a stick is not a pressure trigger — but **tapping fire** on the *other* spy's stick explodes it (`CS:37ce`), and then the **tapper** is the victim (0x12) while the owner laughs (0x0C) (read + 2 recorded self-pickups and 3 recorded duds) | `pickup`'s tag branches |
| the stick's sprite | bit 7 sends every byte to the shared record `DS:0x6F80`, so a planted stick draws as a **mound**; a stick dropped in the open (`0x08`) draws `DS:0x2D38[8]`. The explosion has no sprite of its own — the victim's frame set 0x1B4 *is* the effect (read + both viewports pixel-identical on both recorded blast frames) | nothing to draw; the existing object and spy layers do it |

**What is deliberately not the original's**: `--carry-at TICK:CODE` (and `--carry2-at`) is a demo
aid that puts an item in a spy's hand mid-run, so a panel can plant a stick with one hand and
detonate it with the other; no island's starting kit has both. Nothing else in this stage deviates.

## Stage 14: body heat, the bar, sinking and the freeze

`recon/round/port_check/STAGE14_REPORT.md` (the disassembly of `FUN_26c0_21bd`, `-1e0f` and
`-2238`, the 30-step recorded heat series, the 4,775-dump sink histogram and the demos). Heat is a
per-spy word counted **as cold**: 3 is a full bar and the spy freezes above `0x3D`.

| observation | what the original does (evidence level) | port |
|---|---|---|
| the bar drains outdoors | `CS:0745`: a counter (`0x2F54`) adds 1 a frame; at `0x8A` (138) `FUN_26c0_21bd` adds **2** cold and clears the counter. Nothing else drains on open ground (read + `freeze-death`: 30 steps of 2, one every 15.0 s, i.e. 138 frames) | `heat_block`, `heat_step`, `SpyState::heat` |
| every terrain has its own rate | `FUN_26c0_1e0f` switches on `FUN_26c0_366e`'s code at the cell the spy is *stepping onto*: **water** (1, 0x17) and **deep snow** (2) also sink the sprite (`0x2F68`), deep snow is inert while the **snowshoes are in hand**; **ice** (3) rolls the PRNG for a slip; the **fire** (6) stops the spy and warms him; the **igloo floor** (0xC, 0x1D) warms slowly (read + measured: 4,775 dumps, every `0x2F68` on the model's ramp for its code) | `terrain_tick`, `SpyState::pose_clip`, `terrain::` codes |
| you can freeze to death | `CS:0745` → `FUN_26c0_2238`: the spy is stopped, what he holds is dropped, the pose set becomes `0x72`, the **other** spy laughs and he plays state `0x14` (pose set `0x228`, **56 frames**). When it ends `0x2F64` = 2 and the room passes to the survivor — no respawn, no life, no score (read + `freeze-death` ds_00033..37 replayed) | `freeze_death`, `finish_anim`'s `STATE_FREEZE` arm |
| the bar itself | `0x21bd` composes rows of a two-colour art row into a per-player bitmap and blits it at `0x2F5C`; written as pixels: `min(heat - 3, 59)` drained rows from the top of a **59-row** gauge at x 58..66, rows 9..67 / 117..175. `heat = 62 - value` as the ROI records it (read + 50,134 of 52,470 recorded bar reads reproduce exactly) | `draw_heat_bars`, lifted from the plate's own pixels |

**What is deliberately not the original's**: `--heat N` is a demo aid that seeds both heat words, so
a panel can show the bar at a chosen fill or the freeze without waiting 138 frames a step. The ice
slip's roll uses the port's own PRNG (the original's `FUN_26c0_20cd` takes DH/DL from the DOS clock,
so its stream is not recoverable); the **threshold** (`> 0xE6`, 25 of 256) and the consequence are
the code's.

## Stage 15: the rocket, and every end of round

`recon/round/port_check/STAGE15_REPORT.md` (the disassembly of `FUN_26c0_20ee`, `CS:2156`, `CS:21ae`
and the outer loop at `CS:060d`, the 20 launch dumps, the recorded framebuffer diff). A successful
launch is **not** a one-frame cut: the rocket climbs for **119 frames** and then the round ends.

| observation | what the original does (evidence level) | port |
|---|---|---|
| the rocket climbs | `CS:2065` → `CS:28ae`: `0x2F70` = 1 on the launcher, `0x2F64` = 2 and `0x2F7E` = 1 on the other spy. `FUN_26c0_20ee` then adds **2** to `0x2EBC` a frame and blits the 160-row record at `DS:0x61FA` at bottom `0x36` with shift `0x9E - 0x2EBC`, plus the flame (`DS:0x2D34`/`0x2D36`, alternating on `0x2EBC & 2`) from `0x96` (read + measured: every launch dump reads `0x2EBC` 2 then 238) | `Game::ascent_tick`, `Game::draw_rocket`, `round::{ascent_place, rocket_record, flame_record}` |
| the round ends when it is out of the hatch | `CS:21ae`: at `0x2EBC` = `0xEE` (238) `0x248A` goes up; `CS:0649` turns that into "reset and back to the options screen". 119 frames — 12.85 s at the measured 108 ms, against 12.90 s of recorded frames | `over_flag`, `RoundEnd::Launch` |
| the clock at 00:00 | `CS:065e`: the round ends only when `0x324D` is 0 **and** both `0x2F18` words are clear **and** both `0x2F70` words are clear — it waits for the statues and for the rocket | `round_end_check`, `RoundEnd::Clock` |
| both spies frozen | `CS:06c0`: `0x2F64` and `0x2F66` both >= 2 goes straight back to the options screen, with no timer wait (`freeze-death`: the second statue's animation ends and the same instant's frame is already the options panel at 00:00) | `round_end_check`, `RoundEnd::BothOut` |
| what the loser sees | nothing special: the surviving spy keeps his console, the other console is the flat visitor plate, and the round then ends on the options screen. No win screen, no score, no lives, no round-won text — the only post-round screen is the options screen with the clock at 00:00 (read + the options-panel frames of `playtest3`, `freeze-death` and `rocket-room-2`) | `Phase::Over` → the selector (`play_app`) |
| the difficulty setting | the options screen's zone steps `DS:0x31BF` by 2, 0..8 (`CS:1066`), and the round reads that word itself; its three tables (`0x316C` placement/event thresholds, `0x31CB` a cold threshold, `0x31E1` per-difficulty art) all gate paths this port does not model yet. It does **not** change the clock: `rocket-room-4-hard` ran the same island at difficulty 1 and 5, 480 s both ways (read + measured) | `RoundOpts::difficulty` → `DS:0x31BF`; the tables are an open item |

**What is deliberately not the original's**: `--heat2 N` (seed one spy's heat, so a panel can freeze
one spy while the other plays) and `--parts` (all three parts in the briefcase) are demo aids, named
as such where they are declared. The ascent's counter advances in the frame, not in the drawing pass
where the original counts it — a redraw must not move the rocket.

## Stage 16b: the snowball

`recon/round/port_check/STAGE16B_REPORT.md` (the disassembly of `FUN_26c0_2f87`, `CS:303c` and
`CS:30c9`, the corpus's 46 throws / 48 knock-outs / 60 flights, and 1,938 dump/frame pairs against
`playtest5`'s recorded framebuffer). The throw is **eight drawn frames**, the ball flies **flat** at
three world bytes a frame, and a hit is a **two-frame** flinch.

| observation | what the original does (evidence level) | port |
|---|---|---|
| the gesture | button B with an empty hand (`CS:2d7e` → `FUN_26c0_2f87`): both visitor flags set (both spies on one screen), the other spy still in the round (`0x2F64 < 1`), no ball of mine up (`0x2F40`), and the ground under me not in {climb 1, deep snow 2, ice 3, water `0x17`}; a LEFT/RIGHT **press flag** picks the direction and is consumed. It costs **nothing**: no snow, no item, no heat, no clock (read + measured: 46 of 46 throws) | `Game::throw_snowball`, `BALL_REFUSED_TERRAIN` |
| the release | the throw is state 2, pose set `0x88`, ceiling 14 = **eight** frames; `CS:09b1` sets `0x2F40` when `0x32CF == 2 && 0x2EFC == 8` — the **fifth** drawn frame; the throw itself writes only the three ball words | `BALL_LAUNCH_FRAME`, the release in `advance_anim` |
| the flight | `CS:303c`, run from the frame body's **draw** pass: the row is `y - 0x12` written once (no arc, no gravity), the column steps `0x32FB` = ±3 a frame, and the ball dies when its column leaves the thrower's window `[view - 2, view + 0x2C)` or its row is already past `0x35`. Drawn as the 6-row x 2-byte record at `DS:0x6F66`, in the console that owns the room (read + measured: row 46 of 46, launch column 46 of 46, every step a whole velocity, longest flight 36 bytes) | `Ball`, `snowball_step`, `round::ball_record`, `strip_for` |
| the hit | column in `[his_x + 1, his_x + 4]`, row in `[his_y - 0x18, his_y - 0x0E]`; then immune if climbing (`0x2F68`), indoors (`0x329F >= 0xA`) or running a state at or above `0x0C` (read + measured: 48 knock-outs) | `snowball_hits` |
| the outcome | `CS:30c9`: `0x32CF = -1`, the ball consumed where it lands, the cue `0x9F60` and sound id `-2` recorded, and `0x16` (the ball meets his face) or `0x18` (it takes him from behind) chosen by `0x2F14 + 0x32FB` — pose sets `0x2AC`/`0x2B0`, ceiling 2, **two frames**. Nothing dropped, nothing scored, no heat, no object byte; and a spy who is **throwing** can be hit (`277c`: `0x16`/`0x18` preempt any state at or below 2) (read + measured: 44 x `0x16`, 4 x `0x18`) | `snowball_knock_out`, `start_anim`'s preemption |
| the pixels | 92 ball boxes: **70 pixel-exact**, 16 exact one game frame out, 6 in frames whose whole room already differs by >= 354 px; the throw, ball, knock-out and control groups all have a median of **0** differing pixels in both viewports (measured) | `play/stage16b_frames.py`, the `ball` object in `--state` |

**What is deliberately not the original's**: a throw takes **two frames** in the port — fire, then
fire with a direction — because the original's two joystick buttons share the port's one fire key;
measured, 2 of the corpus's 46 throws cannot be replayed for that reason (the tap before the throw
picks up an item under the spy); ball-versus-ball and ball-versus-object collisions are not modelled
because the routine tests only the other spy and the window's edge; `0x32F7` (written `0` by the
throw, read nowhere) is not carried; the sound cues are recorded as constants and not emitted.

## Stage 17: the computer opponent (1-player mode)

`recon/round/port_check/STAGE17_REPORT.md` (the disassembly of `CS:129c`, `CS:1ca9`, the six
behaviours and `164c`/`1774`, the 192-byte action table and the three difficulty tables, 1,898
recorded AI frames, and the 70-run soak; the second pass, `STAGE17B_REPORT.md`, fixed the wall stall and the missing
deposits). One-player mode is the original's own routine, driven by
the original's own tables, producing the same key flags a human's press produces.

| observation | what the original does (evidence level) | port |
|---|---|---|
| where it plugs in | `CS:0325`: with `DS:0x31DD == 1` (the options screen's player count, copied from `0x31DB` at GO!) player 2's keyboard copy is **skipped** and `CS:129c` runs for `SI = 2`. The computer is **always the black spy**. The attract rounds (`0x3203 != 0`) are a recorded-input replay and never reach the AI (read + measured: `m1-fix-960`'s 221 one-player dumps are all demos) | `RoundOpts::cpu`, `Game::cpu_frame`, the setup screen's one-player option, `--cpu1`/`--cpu2` (a `--cpu` flag puts both spies in the round; `--cpu2 --players 0` is the original's one-player round) |
| the driver | clears its six flags, returns while animating or out, counts `0x3243` from 10 and decides at once when all six intent words are zero; then three hooks (`1559` hand → stow + abandon the walk, `144d` what is under the feet, `1366` the fidget) and a six-way dispatch: map, item, grab, nav, attack, warm (read) | `Cpu::think`, the six `beh_*`, `look_for_item`, `fidget`, `leave_the_walk` |
| the goal | `1ca9`: parts **in the case** (`0x2EB8 & {0x40,0x80,0x200}`) → `0x30` for a shared screen, `0x60` under a minute, `1`/`count` for black's hand, `4×` for white's, `0x10×(parts-1)` → the **192-byte table at `0x30AC`**: 0/1 attack, 2..5 walk, 6 warm. Measured: **316/316** fresh decisions exact, d1 71/71 and d3 245/245 (read + measured) | `Cpu::decide`, `ACTION_TABLE` |
| the errands | the round start records five links — the hatch, the three parts, the briefcase (`0x3219`/`0x3205`/`0x320F`/`0x3223`). Nav walks to them; `164c` steers by band, column and row and finds the door by level code (10 down, 2/24 up, x = gate + 11); `1774` side-steps an object-byte wall and presses A on the hatch's own bytes (read + measured: 93.2 % key agreement, 1,768/1,898) | `World::links`, `door_route`, `avoid`, `beh_nav` |
| the tools | item behaviour: stow, step off, open the panel, step the cursor to `0x2F98`, A; the slots are bucket 4, saw 2, dynamite 0xA. Grab: press A with a free hand, arm the put-away latch with a tool. Warm: find the igloo whose interior row holds the `0x16` marker, walk to its door, in via `0x1B`/`0x30`/`0x1C`, then stand at the fire (read + measured: the recorded AI deposits and reaches 0x6C0 in three sessions) | `beh_item`, `beh_grab`, `beh_warm`, `igloo_door` |
| the difficulty | `0x31BF` = `2*(difficulty-1)` and is only an index. `0x316C` = 180, 205, 225, 250, 254 — a random byte must reach it, so by the code (word 0 = 180 for difficulty 1) the AI notices items and dodges walls **more** at difficulty 1 (29 %) than at difficulty 5 (0.6 %); stage 17 had this direction inverted in its prose (17b §5.1). `0x31CB` = 42..34 — the cold word at which it drops everything and warms up, 8 counts earlier at d5. `0x31E1` is round-start art and is never read by the AI (read + measured) | `DIFF_THRESHOLDS`, `DIFF_COLD`, `DIFF_ART`, `World::thresholds`/`cold` |
| the soak (17b) | `play/stage17b_soak.py`: 7 islands × 5 difficulties × both colours × 3 seeds, full rounds, idle opponent and two-computer modes: **186/210 spy-runs deposit** (stage 17: 0/70), 152/210 all three parts, **143/210 rounds launch**; stalls median 2.71 s, p90 4.33 s, longest 6.3 s against the recording's 2.43 / 3.0 / 3.9 s; 0 dithers | `play/stage17b/`, `STAGE17B_REPORT.md` §1.4 |

**What is deliberately not the original's**: the AI's button A is the port's fire tap and its button
B is fire + a direction (the original's two joystick buttons share the port's one fire key; every B
press the AI makes carries a direction, so replaying recorded AI rounds is unaffected); `--cpu1`
(white computer) is port-only, since the original never calls the routine with `SI = 0`; a
**stray trapulator panel is closed** (the AI can open one with two of its own A presses and nothing
in `CS:129c` closes it — 4,139 frames in the panel measured before the guard); the PRNG
is the port's own (`--seed`), so draw-gated branches agree only in expectation.
**Stage 17b removed** the engine's terrain veto from the wall branch (the original's AI cannot see
terrain; `DS:0x2FA4` is the other spy's body) and resolved `0x2F8C` (= "has taken the briefcase",
`0x2F8A + si`) and `0x2F96` (read as `0x2F96 + si`, i.e. `0x2F98` for black: the trapulator slot the AI asked for); the white
computer reads its own hand and its own plunger/bucket codes (port-only).

**Open** (`STAGE17B_REPORT.md` §5): the port stands still more than the recording at difficulty 1
(36.9 % vs 8.2 %; d3 agrees) and stage 17's prose had the `0x316C` direction inverted (word 0 =
180 = difficulty 1) — the d2/d4/d5 recordings (stage 17c, blocked) would settle it; two computers
brawl for ever across a rock (the attack behaviour has no obstacle pass); the map behaviour's
24-frame look has never been seen on a recorded frame; the AI's sound cues are not emitted
(`0x34A0/0x3496` is the sound-cue block of the `CS:41a4` interrupt handler).

## What this does not do

- **Items** are in (stages 7, 8, 13a and 13b): pick up, dig a mound, drop, bury, plant a stick,
  detonate it, the trapulator's slots and its flashing cursor, the kit box's found/flashing cells,
  the carried/deposited words, the saw, the water bucket and the ice pick with the hole, spill and
  icicle they leave, and the dynamite's explosion.
  **The snowball** is in (stage 16b): the eight-frame throw, the flat flight drawn as its own 6x2
  record in the console that owns the room, and the two-frame knock-out — in the face (`0x16`) or
  from behind (`0x18`). `CS:1ca9`'s per-frame selectors are not modelled —
  they turn out to be the **sound/music index** (`0x2F90`, read by the step-sound code at
  `CS:1574`/`0x17fa`), not a score; see the report §7.
  A spy who reaches the hatch with all three parts **launches**: the rocket climbs out of the hatch
  over 119 frames (stage 15, byte-exact against the recorded framebuffer on the launch frame) and
  the round then ends on the options screen.
- No sound (the cues are recorded as pointers — see the report §2.6 — not emitted), no snow
  *falling*, no storm, no score or lives: the round is movement, doors, the clock, the room
  viewport, the items, the tools, body heat, the freeze, the launch and its ascent, and the three
  ways a round ends.
- **The frame period is the original's** (stage 16a): 26 ticks of the reprogrammed 240 Hz timer,
  108.34 ms, measured at 108.2-108.5 ms in the steadiest sessions and 975.0 ms a clock count over
  2,155 recorded counts. Every rate that used to be quoted per 100 ms frame is now right per second
  as well: heat 138 frames = 14.95 s (8.0 cold a minute), the ascent 119 frames = 12.89 s (12.90 s
  recorded), a 360-count round 351 s. See `STAGE16A_REPORT.md` §4 for the before/after table.
- The **HIDE sign**'s full-bar redraw (the one case where the recorded bar disagrees with
  `DS:0x2F58`) and the between-rounds "PLAYER 1 / GO!" panel are not modelled; both are named.
- The console surround and the clock glyphs are **drawn from the player's files**
  (`src/chrome.rs`): `TRAP.PIC` for the plate, `spy.exe`'s records for every widget. Nothing is
  frozen at a capture's modal state any more.
- Placement in the intro is **executed** from `spytitl.exe`'s own script (ADR 0004), not measured
  from recordings. The two objects seen through the cave mouths are not composited yet.

