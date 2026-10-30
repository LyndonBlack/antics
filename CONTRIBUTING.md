# Contributing

Thanks for your interest in `antics`.

## Ground rules

1. **Never add original game material.** No game files, screenshots, sprite sheets, frame dumps,
   recordings, sound captures, or bytes copied from the original's executables — not in source, not
   in tests, not in docs, not in a pull request description. Facts about the original's code
   (addresses, offsets, routine behaviour, constants) are fine; data is not. The export audit
   (`scripts/export-public.sh --audit-only <dir>`) checks for the obvious cases.
2. **Rules come from evidence.** Behaviour should be traceable to the original's code or to a
   measurement of it running. Where it is a guess, say so and name the evidence level.
3. **Tests must run without the original.** Tests that compare against the original's files or
   recordings must skip themselves when those are absent (see `src/orig.rs`); CI never has them.
4. **No new warnings.** `cargo build --release` is warning-free and should stay so.

## Building and testing

```bash
sudo apt install build-essential pkg-config libsdl2-dev   # Debian/Ubuntu
cargo build --release
cargo test --release
```

## Licence of contributions

By submitting a contribution you agree that it is licensed under the project's licence,
**GPL-3.0-or-later**.
