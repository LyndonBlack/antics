//! **Is this run allowed to talk?** — the process-wide verbosity switch behind `--verbose`.
//!
//! v0.1.2: the program is quiet by default. Everything it used to say about what it was doing —
//! the intro's timing, the window size, the speaker, the selector's keys, every pickup, drop, room
//! change and panel timeout — is **diagnostic chatter**, and it is now printed only when the player
//! asks for it with `--verbose` / `-v`. The switch is set **once**, in `main`, right after the
//! command line is parsed; nothing else may flip it.
//!
//! What does **not** go through here, deliberately:
//!
//! * every error and every usage/argument error (stderr, unchanged, same exit codes);
//! * `--install` and `--where`'s own output — the player asked for it;
//! * every flag whose *purpose* is output (`--state`, `--shot`, `--layout`, `--dump-objects`,
//!   `--tick-trace`, `--state-every`, `--sound-dump`, `--cpu-probe`, the `--scene round --ds`
//!   report, ...). A flag that exists to print something still prints without `--verbose`.
//!
//! So a script's contract is unchanged: it sees the lines it asked for and nothing else, and a
//! player double-clicking the binary sees nothing on a terminal they did not ask to read.

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;

/// The switch. Off unless `-v` / `--verbose` was on the command line (or a test turned it on).
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Turn the chatter on or off. Called once, from `main`, after argument parsing.
pub fn set(on: bool) {
    VERBOSE.store(on, Ordering::SeqCst);
}

/// Is the chatter on? `note!` asks this before printing anything.
pub fn on() -> bool {
    VERBOSE.load(Ordering::SeqCst)
}

/// How many notes this process has emitted — the unit tests' witness that `note!` is silent while
/// the switch is off (and that it prints when it is on). Compiled out of the binary.
#[cfg(test)]
static NOTES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn count_note() {
    NOTES.fetch_add(1, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn note_count() -> usize {
    NOTES.load(Ordering::SeqCst)
}

/// **A line of progress, help or status — printed only with `--verbose`.**
///
/// This is `println!` behind [`on()`], nothing more: the text, its place on stdout and its order
/// relative to other notes are exactly what `println!` did before v0.1.2, so `-v` restores the old
/// output line for line. Errors do **not** use it (they stay on stderr, unconditional); neither does
/// the output a CLI flag exists to produce.
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => {{
        if $crate::verbose::on() {
            #[cfg(test)]
            $crate::verbose::count_note();
            println!($($arg)*);
        }
    }};
}

/// The same, for the few notes that were on **stderr** before v0.1.2 (the placement fallback in
/// `game.rs` and the two "the panel's idle timeout is inert" lines). `-v` restores them exactly as
/// they were, stream included; errors do not need this (they never consult the switch).
#[macro_export]
macro_rules! note_err {
    ($($arg:tt)*) => {{
        if $crate::verbose::on() {
            #[cfg(test)]
            $crate::verbose::count_note();
            eprintln!($($arg)*);
        }
    }};
}
