//! Where this project's own copy of the original lives, and how a test says so when it cannot.
//!
//! The port ships **no** original data: not the game's files and not the art that was measured
//! from it. Every test that reads either one therefore has to be able to stand down on a machine
//! that does not have them, or a clean checkout of the engine cannot even run `cargo test`.
//!
//! Two switches decide that, and both live here so there is exactly one answer:
//!
//! * the file simply is not there (`../../spy-vs-spy-iii/original/256/spy.exe`, a recording under
//!   `../../recon/round/sessions`, ...) -- a public checkout;
//! * `ANTICS_HIDE_ORIGINAL=1` is set -- the file *is* there, and the test must behave as if it were
//!   not, so that the "clean checkout" path can be exercised on the machine that has the original.
//!
//! `ANTICS_ORIGINAL_DIR` redirects where the original is looked for (see [`override_dir`]).
//!
//! A gated test returns early through [`skip`], which prints
//! `skipped: original game data not present (<what>)` and passes. Nothing else about it changes:
//! run on this machine, with the original in place, every one of them still runs and asserts.

use std::path::PathBuf;

/// The 256-colour CGA release, as unpacked from the player's own copy.
pub const ORIGINAL_DIR: &str = "spy-vs-spy-iii/original/256";

/// Paths are relative to the workspace root (`ports/spy-vs-spy-iii` is `CARGO_MANIFEST_DIR`).
pub fn repo() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| manifest.join("../.."))
}

/// `ANTICS_HIDE_ORIGINAL=1` makes every gate answer "not present".
pub fn hidden() -> bool {
    std::env::var_os("ANTICS_HIDE_ORIGINAL").is_some_and(|v| v != "0" && !v.is_empty())
}

pub use crate::install::override_dir;

/// A file under the workspace root, if it is readable and not hidden.
pub fn file(rel: &str) -> Option<PathBuf> {
    if hidden() {
        return None;
    }
    if let (Some(dir), Some(rest)) = (override_dir(), rel.strip_prefix(ORIGINAL_DIR)) {
        let p = dir.join(rest.trim_start_matches('/'));
        return p.exists().then_some(p);
    }
    let p = repo().join(rel);
    p.exists().then_some(p)
}

/// A file of the player's own copy (`spy.exe`, `DEMO.DAT`, ...).
pub fn original(name: &str) -> Option<PathBuf> {
    file(&format!("{ORIGINAL_DIR}/{name}"))
}

/// **An oracle**: one of the ten data files under this checkout's `assets/`, which the recon
/// measured from the original running. The engine never reads them (stage 29 removed every
/// loader); a test uses them only to check what the engine derives, and skips when they are not
/// there, which is every public checkout and every export.
pub fn oracle(name: &str) -> Option<Vec<u8>> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets").join(name);
    std::fs::read(p).ok()
}

/// The path a test *needs*: present here, or the test forgot to gate itself. Panicking with this
/// text is deliberate -- a missing gate is a bug in the test, not a reason to pass quietly.
pub fn require(rel: &str) -> PathBuf {
    file(rel).unwrap_or_else(|| {
        panic!(
            "{rel} is not reachable, and this test did not gate itself: add \
             `if crate::orig::absent() {{ crate::orig::skip(\"...\"); return; }}` at its top"
        )
    })
}

/// Where the recorded sessions live (the private repository's `recon/round/sessions`).
pub const RECORDINGS: &str = "recon/round/sessions";

/// True when a test cannot do its work: the original cannot be read, the recordings it was measured
/// against are not here, or `ANTICS_HIDE_ORIGINAL` says to behave as if neither were.
///
/// The recordings are part of the predicate on purpose. A tree can hold one without the other — an
/// export sitting next to a neighbouring unpacked copy of the game is exactly that — and a test that
/// gates on `spy.exe` alone would then reach `require("recon/…")` and panic instead of skipping.
pub fn absent() -> bool {
    hidden()
        || !original("spy.exe").is_some_and(|p| p.is_file())
        || !file(RECORDINGS).is_some_and(|p| p.is_dir())
}

/// Stand a test down. One line, the same everywhere, so the log can be counted.
pub fn skip(what: &str) {
    eprintln!("skipped: original game data not present ({what})");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The override redirects `original()` and the `ORIGINAL_DIR`-prefixed lookups, and only those.
    #[test]
    fn the_override_redirects_only_the_original_folder() {
        let dir = std::env::temp_dir().join(format!("antics-orig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spy.exe"), b"x").unwrap();
        // env is process-wide: set, assert, restore in one test so no sibling test sees it
        let saved = std::env::var_os("ANTICS_ORIGINAL_DIR");
        std::env::remove_var("ANTICS_ORIGINAL_DIR");
        let other_before = file("ports/antics/Cargo.toml");
        std::env::set_var("ANTICS_ORIGINAL_DIR", &dir);
        let via_original = original("spy.exe");
        let via_file = file(&format!("{ORIGINAL_DIR}/spy.exe"));
        let other = file("ports/antics/Cargo.toml");
        std::env::set_var("ANTICS_ORIGINAL_DIR", "");
        let empty = override_dir();
        match saved {
            Some(v) => std::env::set_var("ANTICS_ORIGINAL_DIR", v),
            None => std::env::remove_var("ANTICS_ORIGINAL_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(via_original, Some(dir.join("spy.exe")));
        assert_eq!(via_file, Some(dir.join("spy.exe")));
        assert_eq!(other, other_before, "other paths are untouched");
        assert!(empty.is_none(), "an empty value means unset");
    }
}
