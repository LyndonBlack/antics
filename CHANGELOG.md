# Changelog

All notable changes to `antics` are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.2] - 2026-10-31

### Changed
- The program is quiet by default: the informational lines it printed while it ran (the intro's
  timing, the window and speaker, the selector, every pickup, drop and room change, the panel's
  idle timeout) are printed only with `--verbose` / `-v`, which restores all of them. Errors, usage
  errors, `--install`/`--where`'s output and the output of flags that exist to print something
  (`--state`, `--layout`, `--tick-trace`, ...) are unchanged.

### Fixed
- The Linux and macOS instructions, and the messages the program prints, use `./antics`: a binary
  in the folder you unpacked it in is not on `PATH`, so `antics --install ...` failed with
  `command not found`. Windows keeps `antics.exe`. The "data not found" message says once how to
  run it from the folder it came in, or to put it on `PATH`.

## [0.1.1] - 2026-10-31

### Fixed
- Running `antics` with no arguments, or `--play`, now opens the window and plays the intro, selector
  and rounds (0.1.0 silently exited); the Linux release now includes X11/Wayland/audio backends.
- A window that cannot be opened is reported with its cause and a fix hint, and exits non-zero,
  instead of the process ending quietly with status 0 (this includes the case where SDL falls back to
  its `dummy` video driver because no display driver is available).
- `--play` alongside a flag that asks for headless output (for example `--play --shot f.bmp` or
  `--scene intro --play`) is reported rather than silently ignored.
- The intro is rendered in the window in its own CGA palette: the MAXX OUT! card is palette 0, not
  palette 1.

## [0.1.0] - 2026-10-30

First release.

### Added
- A from-scratch Rust reimplementation of the engine of *Spy vs. Spy III: Arctic Antics*, run on
  the player's own copy of the game (`antics --install <folder>`).
- The intro (the original's script engine ported, drawing from the player's `TITLE.DAT`,
  `MAXX.PIC` and `TEXT*.DAT`), the setup selector, and the full round: items, traps, the
  Trapulator, body heat and freezing, snowstorm and deep snow, the map, the rocket launch and the
  hatch, one- and two-player modes, the computer opponent, sound cues (`--no-sound` mutes them).
- Every graphic is drawn at run time by a port of the original's own drawing code; the repository
  and the releases contain no original art, code, sound or data.
- `--install` copies the nine required files, checking each one's size and SHA-256; `--data-dir`
  and `--where`.
- Linux, Windows and macOS release builds (statically linked SDL2).

### Known differences
- See the Status and "What it does not do" sections of the README and `docs/DEVELOPERS.md`.
