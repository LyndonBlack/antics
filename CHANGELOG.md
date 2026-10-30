# Changelog

All notable changes to `antics` are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

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
