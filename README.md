# antics

A native reimplementation of the **engine** of *Spy vs. Spy III: Arctic Antics* — MS-DOS
game — in Rust. It runs your own copy of the original: it reads the original's executables and data
files at run time, reimplements their behaviour from the disassembly, and draws every screen itself,
with a port of the original's own drawing code.

**This repository contains no original game code, art or sound.** No game files are included, here or
in the releases. You need to own the game.

> ## You need the original game
>
> `antics` does nothing without the nine files of your own copy of *Spy vs. Spy III: Arctic Antics*
> (`spy.exe`, `spytitl.exe`, `TITLE.DAT`, `TRAP.PIC`, `MAXX.PIC`, `DEMO.DAT`, `TEXT1..3.DAT`). Point
> it at your copy once and it copies them, after checking each one's size and SHA-256, into a
> per-user data directory:
>
> ```bash
> # from the folder you unpacked antics in (Windows: antics.exe ...)
> ./antics --install ~/svs3    # a folder containing your unzipped original
> ./antics                     # play
> ```
>
> Sizes, sha256 hashes of the supported release, per-OS instructions, controls, troubleshooting and
> the copyright position: **[docs/YOU-NEED-THE-ORIGINAL.md](docs/YOU-NEED-THE-ORIGINAL.md)**.

## What works

- The **`spytitl.exe` intro** — the animated title sequence, frame for frame.
- The **setup selector** — island shape, hidden/revealed launch site, one or two players, difficulty.
- **A playable round**: a spy walks the iceberg on the arrow keys, the room scrolls, doors lead
  between rooms, the round clock runs down, and a round starts from the selector and ends back on it.
- **Items and traps**: pick up, dig, drop, bury, the trapulator, the kit box, the saw, the water
  bucket, the ice pick, the dynamite stick and its detonator, the snowball.
- **Body heat** and the freeze, the **igloo door**, the two spies blocking and pushing each other when
  they share a screen, the rocket launch and the three ways a round ends.
- A **computer opponent** in one-player mode (it is always the black spy, as in the original).

## What it does not do

The sound is the original's cues played through a simple tone generator, not the PC speaker hardware itself (`--no-sound` mutes it). Known
small differences from the original, each with its evidence, are listed in `docs/DEVELOPERS.md`.

## Install

Download the archive for your platform from this repository's **Releases** page, unpack it, then:

| OS | install | run |
|---|---|---|
| Linux | `./antics --install ~/svs3` | `./antics` |
| Windows | `antics.exe --install .\svs3` | `antics.exe` |
| macOS | `./antics --install ~/svs3` | `./antics` |

Run these from the folder you unpacked it in (or put `antics` on your `PATH`, e.g. move it to
`~/.local/bin`). On Linux and macOS a binary in the current directory is not on `PATH`, so the
`./` is part of the command; Windows searches the current directory and the file is `antics.exe`.

`~/svs3` is any folder that holds your unzipped copy of the game. `--install` copies its nine files
into a per-user directory: `~/.local/share/antics` (Linux, or `$XDG_DATA_HOME/antics`),
`%APPDATA%\antics` (Windows), `~/Library/Application Support/antics` (macOS). `--data-dir DIR`
overrides it for one run. If the data is missing, the program says so and tells you the command to
run, spelled the way that works on your platform (`./antics --install ...` on Linux and macOS).

Every start checks the size and SHA-256 of those nine files before drawing anything, so a modified,
truncated or incomplete copy is named on stderr (exit code 8, with the `--install` command to fix it)
instead of being decoded into a broken screen; see
[YOU-NEED-THE-ORIGINAL.md](docs/YOU-NEED-THE-ORIGINAL.md#troubleshooting).

Run it with no arguments (or `--play`) to play. It opens the window and plays the
original's own sequence: the intro — the MAXX OUT! card, then the animated arctic title scene, which
ends by itself, or immediately on `Return`, `Space`, `Esc` or a click — then the setup selector, then
the rounds, each of which returns to the selector. Closing the window quits at any point.

### Windows installer

On Windows there is also an installer, `antics-0.1.3-windows-x86_64-setup.exe`, on the Releases page.
It installs for the current user only (no administrator rights), offers **Start Menu** and desktop
shortcuts, and does the `--install` step for you:

1. run the setup program;
2. choose where to install antics (the default is fine);
3. choose the folder that holds your own copy of the game — the one with `spy.exe` and `TRAP.PIC` in
   it — or leave the box empty and run `antics.exe --install <folder>` later;
4. finish, and start antics from the shortcut.

The shortcut runs the game with no arguments, so no console window appears beside it, and a start-up
problem (the game files not found, a window that cannot open) is reported in a message box rather
than in a console that would close with the process. If you would rather see the console output, run
`antics.exe --verbose` from `cmd`.

SmartScreen may say **Windows protected your PC**, because the installer is not code-signed: choose
**More info** → **Run anyway**. Like the rest of the release, the installer contains the program
only — never the original game files.

The binaries are unsigned. On macOS, right-click → **Open** (or
`xattr -d com.apple.quarantine ./antics`); on Windows, **More info** → **Run anyway**. Details in
[the original-game doc](docs/YOU-NEED-THE-ORIGINAL.md#troubleshooting).

## Build from source

SDL2 is needed for the windowed front-end only. On Debian/Ubuntu:

```bash
sudo apt install build-essential pkg-config libsdl2-dev
cargo build --release          # ./target/release/antics
cargo test --release           # tests that need the original game skip themselves
```

To build without depending on a system SDL2, use the release features (they need `cmake` and a C
compiler instead):

```bash
cargo build --release --features bundled,static-link
```

`bundled` compiles the SDL2 source that ships with the `sdl2` crate; `static-link` links it
statically. Neither changes what the program does.

## Controls

| | move | fire / action |
|---|---|---|
| Player 1 (white spy) | arrow keys | `RETURN` |
| Player 2 (black spy) | `A` `W` `D` `X` (`X` is down) | `Q` |

`Esc` leaves the round, `R` restarts it, `F1` toggles the spy marker, `F2` writes a BMP of the
current frame. In the setup selector, arrows move the ring and `RETURN` changes the option; on `GO!`
`RETURN` starts the round.

The intro comes first and plays by itself; `RETURN`, `Space`, `Esc` or a click skips it. `Esc` on
the selector leaves the game, and closing the window quits wherever you are.

A tap of fire is the original's button A. Holding fire and pressing a direction is its button B:
pick up, put away, dig, bury, open the trapulator, or throw a snowball — standing on an item and
tapping picks it up (a mound means dig), tapping again puts it away, and holding fire with a
direction buries it. The full gesture table, with the original's routines, is in
`docs/DEVELOPERS.md`.

## Command line

The window is the normal way to play; the other flags exist because the port is verified against
recordings, and a few of them are handy. A command line made only of window and data options
(`--scale`, `--data-dir`, `--no-sound`, `--fps`, `--panel-auto`, `--verbose`, ...) plays too;
anything that asks for headless output (`--shot`, `--state`, `--frame`, `--scene`, `--script`,
`--demo`, `--install`, `--where`) keeps its own meaning and does not open a window.

The program is **quiet**: during play it prints nothing at all. `--verbose` (`-v`) brings back the
lines it uses to describe what it is doing — the intro's timing, the window and speaker, what
happens in the selector, every pickup, drop and room change, the panel's idle timeout. Errors and
the output of a flag that exists to print something (`--state`, `--layout`, `--install`, ...) are
printed either way; if something seems wrong, run with `-v` and the reason is usually one of those
lines.

| flag | meaning |
|---|---|
| *(none)* | play: the intro, then the selector, then the rounds |
| `--play` | open the SDL2 window (3x integer scale, 960x600) — the same as no arguments |
| `--verbose`, `-v` | print what the program is doing (off by default: a plain run prints nothing) |
| `--scale N` | window scale, 1-8 |
| `--data-dir DIR` | use a different data directory for this run |
| `--scene setup --play` / `--scene round --play` | start on the selector / straight into a round |
| `--island 0-6` | iceberg shape |
| `--launch 0\|1` | launch site hidden (0) or revealed (1) |
| `--players 0\|1` | one spy (the original's default) or both |
| `--difficulty 0-4` | shown as 1-5 |
| `--seed N` | pin the round's object layout and start cells |
| `--frame N` | render one intro state headless, 0 (the card) to 1841 (the last one) |
| `--shot FILE` | write the frame as a 24-bit BMP, 320x200 |
| `--state FILE` | write the frame's state as JSON |
| `--fps F` | override the frame period (the original is 9.2298 fps) |

`docs/DEVELOPERS.md` documents the rest — scripted replays, recorded-dump comparisons, the demo
aids — for anyone working on the port.

## Status

Version 0.1.3. The intro, the setup selector and the round are playable end to end for one or two
players, and are verified against recordings of the original running. It is unofficial and
non-commercial; see [docs/NOTICE.md](docs/NOTICE.md).

## Documentation

| file | contents |
|---|---|
| [docs/YOU-NEED-THE-ORIGINAL.md](docs/YOU-NEED-THE-ORIGINAL.md) | what game files are needed, the exact release, install, troubleshooting, copyright |
| [docs/DEVELOPERS.md](docs/DEVELOPERS.md) | how the port works: subsystems, constants, timing, the full CLI, the evidence |
| [docs/NOTICE.md](docs/NOTICE.md) | non-affiliation, no-assets statement, trademarks, licence status |

## Disclaimer

**You must supply the original game.** `antics` contains none of *Spy vs. Spy III: Arctic Antics* —
no program code, no graphics, no sound, no data of any kind, in this repository or in any release.
To run it you must provide the original game files from a copy you are entitled to use;
`antics --install` copies nine of them into your own user directory and checks each against a known
size and SHA-256. Nothing is downloaded and nothing is converted. Obtaining a lawful copy is your
responsibility.

**This is a from-scratch reimplementation.** `antics` is original source code written in Rust.
It is not a fork or port of any existing engine, emulator or SDK, it links no third-party game
engine (the only runtime dependency is the SDL2 library for the window, input and audio output), and
it was not built from any leaked or proprietary source code. Its behaviour was worked out by
studying the original program and measuring recordings of it running; the notes on how are in
`docs/DEVELOPERS.md`.

**No affiliation, no warranty.** This project is unofficial and non-commercial. It is not affiliated
with, authorised by or endorsed by the rights holders of the game or of the *Spy vs. Spy* property.
*Spy vs. Spy* and all related names are trademarks of their respective owners, used here only to
identify what this code reimplements. The software is provided "as is", without warranty of any kind
(see the licence); use it at your own risk.

## Licence

Copyright (C) 2026 Mostlysane.

`antics` is free software, licensed under the **GNU General Public License v3.0 or later**
(`GPL-3.0-or-later`); see [LICENSE](LICENSE). You may use, modify and redistribute it under those
terms; any distributed version must remain under the same licence and be accompanied by its source.
The licence covers this repository's code and documentation only — it grants no rights in the
original game, which belongs to its owners and is not included.

Contributions: see [CONTRIBUTING.md](CONTRIBUTING.md). Security and takedown contact:
[SECURITY.md](SECURITY.md).
