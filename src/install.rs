//! `antics --install <folder>`: build the per-user data directory out of the player's own copy of
//! the original.
//!
//! The port ships no game data and no art measured from the game. Everything the engine draws it
//! draws from nine of the player's own files, so all `--install` does is **copy those nine files**
//! into one directory, after checking each one's size and SHA-256. Nothing is generated, decoded
//! or converted: the drawing is done by the engine, at run time, from those files.
//!
//! Identity is checked before anything is written, because a wrong release would otherwise be
//! decoded into plausible-looking nonsense: size plus SHA-256, computed here (no crates, no
//! OpenSSL). Only the digests are compiled in; not one byte of the game does.
//!
//! The layout it writes:
//!
//! ```text
//! <data dir>/spy.exe  spytitl.exe  TITLE.DAT  TRAP.PIC  MAXX.PIC  DEMO.DAT  TEXT1..3.DAT
//! <data dir>/antics-install.json      what was written, and from which originals
//! ```
//!
//! `<data dir>` is `--data-dir`, else `$XDG_DATA_HOME/antics` (or `~/.local/share/antics`),
//! `%APPDATA%\antics` on Windows, `~/Library/Application Support/antics` on macOS.

use std::fs;
use std::path::{Path, PathBuf};

/// One file the engine reads at run time, and the identity of the release this build supports.
pub struct Ident {
    pub name: &'static str,
    pub size: usize,
    pub sha256: &'static str,
}

/// **The nine files the engine reads**, all of them: `spy.exe` (the round, the selector and the
/// chrome), `spytitl.exe` with `TITLE.DAT` / `MAXX.PIC` / `TEXT1..3.DAT` (the intro) and `TRAP.PIC`
/// (the round's and the selector's screen). Computed from `spy-vs-spy-iii/original/256/` on the
/// development machine; `the_embedded_identity_is_the_reference_copy` re-checks every row.
pub const ORIGINALS: &[Ident] = &[
    Ident {
        name: "spy.exe",
        size: 111923,
        sha256: "889d68beee3434047373e21efe8b5148d40ac230836989f10d5546dda4f7889a",
    },
    Ident {
        name: "spytitl.exe",
        size: 48914,
        sha256: "e60cb432379eb8170e7c3c233c8365ad07b7f88b2063e640b22fcb49be0067eb",
    },
    Ident {
        name: "TITLE.DAT",
        size: 16000,
        sha256: "2583590b87b84ea8c39e9e5bae3dc7639a457e5c63964c4d8140f6e496d6a601",
    },
    Ident {
        name: "TRAP.PIC",
        size: 16192,
        sha256: "3a7f0fb94281f4ed882bae7d809cd12e01072e86a2be42aa128d7c04b689736f",
    },
    Ident {
        name: "MAXX.PIC",
        size: 16000,
        sha256: "75ff90f0030483f7b16831b9b498bfddbc58f4e8080e79dbbabf5dc6c870db79",
    },
    Ident {
        name: "DEMO.DAT",
        size: 1882,
        sha256: "2031ff335694f0ddacb81681d25bb3ad146f88ef68b42311327ae120ce18c00d",
    },
    Ident {
        name: "TEXT1.DAT",
        size: 4322,
        sha256: "5df9d6bcd18b276037c18872f56ab12138fc0d2da668dfcf423cf6364bfcb285",
    },
    Ident {
        name: "TEXT2.DAT",
        size: 4322,
        sha256: "be50fafe4747f7cb8a2b2f692af17993275c25346a1df99d6185c2e25396acb8",
    },
    Ident {
        name: "TEXT3.DAT",
        size: 4322,
        sha256: "a7903e5ad2f2c3653f7cc4195c93d4db7e9e71dd0b7494bc25450ad3aa634670",
    },
];

/// The marker that says a directory holds an installed set.
pub const MARKER: &str = "antics-install.json";

// ---- the data directory ---------------------------------------------------

/// `--data-dir`, else the per-user data directory for this platform. `None` when there is no home
/// directory to put it in (a bare container), in which case `--install` must be told where to go.
pub fn data_root(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(d) = explicit {
        return Some(d.to_path_buf());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).or(home)?;
        return Some(appdata.join("antics"));
    }
    if cfg!(target_os = "macos") {
        return Some(home?.join("Library/Application Support/antics"));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("antics"));
    }
    Some(home?.join(".local/share/antics"))
}

/// Whether this directory holds a set `--install` wrote (marker + every file the engine reads).
pub fn is_installed(dir: &Path) -> bool {
    dir.join(MARKER).is_file() && ORIGINALS.iter().all(|id| dir.join(id.name).is_file())
}

/// Where this run reads from: the directory holding the player's nine files.
pub struct Dirs {
    pub originals: PathBuf,
    /// A short name for the choice, printed by `--where` and quoted in the error messages.
    pub origin: &'static str,
}

/// The development path: the unpacked release beside this checkout. **Looked at, never named in
/// a message** -- on a player's machine it does not exist, and a release build must not send them
/// to a directory of ours.
fn dev_original_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `ports/spy-vs-spy-iii` -> the workspace root, without a `..` in the printed path
    let root = manifest.parent().and_then(|p| p.parent()).map(Path::to_path_buf)
        .unwrap_or_else(|| manifest.join("../.."));
    root.join(ORIGINAL_DEV)
}

/// Where an unpacked copy of the game sits in this workspace (`spy-vs-spy-iii/original/256`).
pub const ORIGINAL_DEV: &str = "spy-vs-spy-iii/original/256";

/// The run-time lookup order:
///
/// 1. an explicit `--data-dir`, or an explicit `--assets`;
/// 2. the per-user data directory, when `--install` has written a set there;
/// 3. the development path -- the unpacked original beside this checkout.
///
/// `Err` is the friendly "Original game data not found" text, never a raw `io::Error`.
pub fn resolve(assets_flag: Option<&Path>, data_flag: Option<&Path>) -> Result<Dirs, String> {
    resolve_with(assets_flag, data_flag, data_root(data_flag).as_deref())
}

/// [`resolve`] with the per-user directory passed in, so the lookup order can be tested without
/// writing into the tester's real data directory.
pub fn resolve_with(
    assets_flag: Option<&Path>,
    data_flag: Option<&Path>,
    root: Option<&Path>,
) -> Result<Dirs, String> {
    if let Some(d) = data_flag {
        let originals = assets_flag.map(Path::to_path_buf).unwrap_or_else(|| d.to_path_buf());
        return Ok(Dirs { originals, origin: "--data-dir" });
    }
    if let Some(a) = assets_flag {
        return Ok(Dirs { originals: a.to_path_buf(), origin: "--assets" });
    }
    if let Some(r) = root {
        if is_installed(r) {
            return Ok(Dirs { originals: r.to_path_buf(), origin: "installed data dir" });
        }
    }
    let dev = dev_original_dir();
    if dev.join("spy.exe").is_file() {
        return Ok(Dirs { originals: dev, origin: "development paths" });
    }
    Err(not_found())
}

// ---- identity -------------------------------------------------------------

/// Find one original inside the player's folder: exact name first, then the DOS upper-case
/// spelling a CD or an unzip-to-FAT copy carries.
pub fn locate(dir: &Path, name: &str) -> Option<PathBuf> {
    let exact = dir.join(name);
    if exact.is_file() {
        return Some(exact);
    }
    let upper = dir.join(name.to_ascii_uppercase());
    upper.is_file().then_some(upper)
}

/// Check one file's size and digest.
pub fn check(dir: &Path, id: &Ident) -> Result<PathBuf, String> {
    let Some(p) = locate(dir, id.name) else {
        return Err(format!(
            "{} not found in {}.\nThis is the folder holding your own copy of Spy vs. Spy III: \
             Arctic Antics -- it must contain {} and DEMO.DAT.",
            id.name,
            dir.display(),
            id.name
        ));
    };
    let bytes = fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    if bytes.len() == id.size && sha256_hex(&bytes) == id.sha256 {
        return Ok(p);
    }
    let got = sha256_hex(&bytes);
    Err(format!(
        "this build supports Spy vs. Spy III: Arctic Antics, 256-colour CGA release \
         ({name} {size} bytes, sha256 {want}), and nothing else.\n\
         {path} is {got_size} bytes, sha256 {got}.\n\
         Point --install at the folder with the 1988 release (files dated 1987-1988).",
        name = id.name,
        size = id.size,
        want = id.sha256,
        path = p.display(),
        got_size = bytes.len(),
        got = got
    ))
}

// ---- install --------------------------------------------------------------

/// One file `--install` copied.
#[derive(Debug)]
pub struct Installed {
    pub path: PathBuf,
    pub bytes: usize,
}

#[derive(Debug)]
pub struct Report {
    pub dir: PathBuf,
    /// The player's files, copied -- all nine of them.
    pub copied: Vec<Installed>,
}

/// Copy a file to `dest`, leaving it alone when the bytes are already identical (so a second
/// `--install` changes no timestamps).
fn put(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Ok(old) = fs::read(dest) {
        if old == bytes {
            return Ok(());
        }
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(dest, bytes).map_err(|e| format!("{}: {e}", dest.display()))
}

/// Run the whole install. `src` is the player's folder, `dest` the data directory.
///
/// Two steps and no more: check every one of the nine files, then copy it. Nothing is derived,
/// so a failed check leaves the data directory exactly as it was.
pub fn install(src: &Path, dest: &Path) -> Result<Report, String> {
    if !src.is_dir() {
        return Err(format!(
            "{} is not a folder.\nPass the folder that holds your own copy of the game: \
             {} --install /path/to/spy-vs-spy-iii",
            src.display(),
            command()
        ));
    }

    // 1. identity, before a single byte is written
    let mut found: Vec<PathBuf> = Vec::with_capacity(ORIGINALS.len());
    for id in ORIGINALS {
        found.push(check(src, id)?);
    }

    fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;

    // 2. copy the player's own files
    let mut copied = Vec::with_capacity(ORIGINALS.len());
    for (id, path) in ORIGINALS.iter().zip(&found) {
        let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let out = dest.join(id.name);
        put(&out, &bytes)?;
        copied.push(Installed { path: out, bytes: bytes.len() });
    }

    // 3. the marker, so the run-time lookup can tell an installed set from an empty directory
    let marker = format!(
        "{{\n  \"version\": 2,\n  \"originals\": {},\n  \"source\": {}\n}}\n",
        json_list(&copied),
        json_string(&src.display().to_string())
    );
    put(&dest.join(MARKER), marker.as_bytes())?;

    Ok(Report { dir: dest.to_path_buf(), copied })
}

/// A JSON string literal, for the one free-form value (the folder the files came from).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_list(items: &[Installed]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|i| {
            let name = i.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            format!("\"{}\"", name.replace('\\', "\\\\"))
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

// ---- SHA-256 --------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of a byte slice, as lower-case hex. Standard FIPS 180-4, written out here so that
/// `--install` needs nothing but `std`.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

// ---- the message a player sees when there is nothing to run -----------------

/// **The command a player types**, in the form that works where they are.
///
/// A binary sitting in the folder they unpacked it in is **not on `PATH`** on Linux or macOS, so
/// `antics --install ...` fails with `command not found` there; `./antics --install ...` works. On
/// Windows the current directory is searched, and the released file is `antics.exe`. Program
/// messages therefore spell the command this way — always the relative form, never a guess at the
/// player's `PATH`.
pub fn command() -> &'static str {
    if cfg!(windows) {
        "antics.exe"
    } else {
        "./antics"
    }
}

/// Printed instead of a raw `io::Error` when no original and no installed set can be found.
///
/// **No development path appears here, in any build.** On a player's machine
/// `../../spy-vs-spy-iii/original/256` is meaningless, and a release build must not name a
/// directory of ours; the development lookup still happens, silently, in [`resolve_with`].
pub fn not_found() -> String {
    not_found_looking_in(&[])
}

/// The same text, naming the directories *this* run actually looked in -- the installed data
/// directory and any directory the caller passed.
pub fn not_found_looking_in(also: &[&Path]) -> String {
    let mut where_: Vec<String> = Vec::new();
    if let Some(d) = data_root(None) {
        where_.push(d.display().to_string());
    }
    for p in also {
        let s = p.display().to_string();
        if !where_.contains(&s) {
            where_.push(s);
        }
    }
    let mut msg = format!(
        "Original game data not found.\n\
         Run: {} --install <folder with your copy of Spy vs Spy III: Arctic Antics>\n",
        command()
    );
    // the one thing that makes that command fail on a unix box, said once (a player who ran it
    // from `~/Downloads/antics-0.1.2-linux` got `antics: command not found`)
    if !cfg!(windows) {
        msg.push_str(
            "Run it from the folder you unpacked it in, as shown, or put the binary on your PATH\n\
             (e.g. move it to ~/.local/bin).\n",
        );
    }
    msg.push_str(&format!("(looked in {})\nSee docs/YOU-NEED-THE-ORIGINAL.md", where_.join(", ")));
    msg
}

// ---- tests ---------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A scratch directory under the system temp dir, removed on drop. (Nothing here touches the
    /// player's real data directory: every test passes `--data-dir` explicitly.)
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let p = std::env::temp_dir().join(format!(
                "antics-install-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("scratch");
            Scratch(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn original_dir() -> PathBuf {
        crate::orig::repo().join(crate::orig::ORIGINAL_DIR)
    }

    /// A copy of the player's folder, so a test can damage one file without touching the original.
    fn copy_of_original(tag: &str) -> Option<Scratch> {
        if crate::orig::absent() {
            crate::orig::skip("spy.exe");
            return None;
        }
        let s = Scratch::new(tag);
        for id in ORIGINALS {
            let src = original_dir().join(id.name);
            if src.is_file() {
                let dst = s.path().join(id.name);
                fs::copy(&src, &dst).expect("copy");
                // the reference copy is read-only (0444, as unpacked from the 1988 disk); the
                // scratch copy has to be writable so a test can damage it
                let mut perm = fs::metadata(&dst).expect("mode").permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                perm.set_readonly(false);
                fs::set_permissions(&dst, perm).expect("chmod");
            }
        }
        Some(s)
    }

    // ---- the digest itself -------------------------------------------------

    /// Without this the identity check would only prove that two wrong numbers agree.
    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // one byte longer than a block boundary, so the padding path is exercised too
        let long = vec![b'a'; 1000];
        assert_eq!(
            sha256_hex(&long),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    // ---- identity ----------------------------------------------------------

    /// The digests compiled into the binary are the ones this project's own copy has: a typo here
    /// would reject every player's folder.
    #[test]
    fn the_embedded_identity_is_the_reference_copy() {
        for id in ORIGINALS {
            let Some(p) = crate::orig::original(id.name) else {
                crate::orig::skip(id.name);
                return;
            };
            let bytes = fs::read(&p).expect("original");
            assert_eq!(bytes.len(), id.size, "{} size", id.name);
            assert_eq!(sha256_hex(&bytes), id.sha256, "{} sha256", id.name);
        }
    }

    /// **The table `docs/YOU-NEED-THE-ORIGINAL.md` prints is this table.**
    ///
    /// The document is the only place a player can read the identity of the files the engine
    /// takes, so a copied-then-edited row there is a lie about what `--install` accepts. Every
    /// row has to name the file, its size and its whole digest, in the order the code lists them.
    #[test]
    fn the_docs_list_every_identity_row() {
        let doc = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/YOU-NEED-THE-ORIGINAL.md");
        let Ok(text) = fs::read_to_string(&doc) else {
            eprintln!("skipped: this checkout has no {}", doc.display());
            return;
        };
        for id in ORIGINALS {
            let size = format!("| `{}` | {} | `{}` |", id.name, id.size, id.sha256);
            assert!(
                text.contains(&size),
                "{} is not in the doc table as `{size}` (one row per file, size and full sha256)",
                id.name
            );
        }
        assert_eq!(ORIGINALS.len(), 9, "the engine reads nine files");
    }

    /// A DOS copy of the game is usually upper-case; the lookup has to survive that.
    #[test]
    fn locate_accepts_the_dos_upper_case_spelling() {
        let Some(s) = copy_of_original("upper") else { return };
        fs::remove_file(s.path().join("DEMO.DAT")).expect("rm");
        let id = ORIGINALS.iter().find(|i| i.name == "TITLE.DAT").unwrap();
        assert_eq!(locate(s.path(), "TITLE.DAT"), Some(s.path().join("TITLE.DAT")));
        assert!(check(s.path(), id).is_ok());
        // ... and the missing one is named in the message
        let err = check(s.path(), ORIGINALS.iter().find(|i| i.name == "DEMO.DAT").unwrap())
            .expect_err("DEMO.DAT is gone");
        assert!(err.contains("DEMO.DAT not found in"), "{err}");
    }

    // ---- the failures a player can actually hit -----------------------------

    /// A folder that does not hold the game at all: one line, the file's name, and what to do.
    #[test]
    fn a_folder_without_the_game_names_the_missing_file() {
        let s = Scratch::new("empty");
        let id = &ORIGINALS[0];
        let err = check(s.path(), id).expect_err("nothing there");
        assert!(err.contains("spy.exe not found in"), "{err}");
        assert!(err.contains("This is the folder holding your own copy"), "{err}");
    }

    /// A different release: the message must say *which* build this binary supports, and what the
    /// folder actually holds -- never "invalid data" and never a decode of the wrong bytes.
    #[test]
    fn a_wrong_version_says_which_build_is_supported() {
        let Some(s) = copy_of_original("wrongver") else { return };
        let mut title = fs::read(s.path().join("TITLE.DAT")).expect("read");
        title.push(0); // the Amiga-era files are a different size; one byte is enough
        fs::write(s.path().join("TITLE.DAT"), &title).expect("write");
        let id = ORIGINALS.iter().find(|i| i.name == "TITLE.DAT").unwrap();
        let err = check(s.path(), id).expect_err("16001 bytes is not the CGA release");
        assert!(err.contains("this build supports Spy vs. Spy III"), "{err}");
        assert!(err.contains("16000 bytes"), "{err}");
        assert!(err.contains("16001 bytes"), "{err}");
        assert!(err.contains(&id.sha256[..16]), "{err}");
    }

    /// `install` refuses to write anything when the identity fails.
    #[test]
    fn install_writes_nothing_from_a_folder_it_does_not_recognise() {
        let Some(s) = copy_of_original("refuse") else { return };
        let mut exe = fs::read(s.path().join("spy.exe")).expect("read");
        exe[0x200] ^= 0xFF;
        fs::write(s.path().join("spy.exe"), &exe).expect("write");
        let out = Scratch::new("refuse-out");
        let err = install(s.path(), out.path()).expect_err("a damaged spy.exe must be refused");
        assert!(err.contains("this build supports"), "{err}");
        assert_eq!(
            fs::read_dir(out.path()).expect("out").count(),
            0,
            "a refused install must leave the data directory empty"
        );
    }

    /// **A folder with eight of the nine files is refused, naming the ninth.** The intro is the
    /// default scene, so `TEXT2.DAT` is as necessary as `spy.exe`.
    #[test]
    fn a_folder_missing_one_file_is_refused_by_name() {
        let Some(s) = copy_of_original("one-short") else { return };
        fs::remove_file(s.path().join("TEXT2.DAT")).expect("rm");
        let out = Scratch::new("one-short-out");
        let err = install(s.path(), out.path()).expect_err("eight files is not the release");
        assert!(err.contains("TEXT2.DAT not found in"), "{err}");
        assert_eq!(fs::read_dir(out.path()).expect("out").count(), 0);
    }

    // ---- the install itself -------------------------------------------------

    /// **Every one of the nine files, byte for byte, and nothing else.** The port ships no art and
    /// generates none: the data directory is the player's own folder, copied.
    #[test]
    fn install_copies_all_nine_files_and_nothing_else() {
        let Some(s) = copy_of_original("full") else { return };
        let out = Scratch::new("full-out");
        let rep = install(s.path(), out.path()).expect("install");

        assert_eq!(ORIGINALS.len(), 9);
        assert_eq!(rep.copied.len(), 9, "nine files copied");
        for id in ORIGINALS {
            let p = out.path().join(id.name);
            assert!(p.is_file(), "{} was not copied", id.name);
            let got = fs::read(&p).expect("copied file");
            assert_eq!(got.len(), id.size, "{} size", id.name);
            assert_eq!(sha256_hex(&got), id.sha256, "{} is not the file that was checked", id.name);
        }
        // the marker plus the nine, and no `assets/` directory: there is nothing to put in one
        let mut names: Vec<String> = fs::read_dir(out.path())
            .expect("out")
            .map(|e| e.expect("entry").file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        let mut want: Vec<String> = ORIGINALS.iter().map(|i| i.name.to_string()).collect();
        want.push(MARKER.to_string());
        want.sort();
        assert_eq!(names, want, "the data dir holds the nine files and the marker");
        assert!(!out.path().join("assets").exists(), "no art directory is written");
        assert!(is_installed(out.path()));
    }

    /// Running it twice changes nothing: same files, same bytes, no duplicates.
    #[test]
    fn install_is_idempotent() {
        let Some(s) = copy_of_original("idem") else { return };
        let out = Scratch::new("idem-out");
        let first = install(s.path(), out.path()).expect("install");
        let mut before: Vec<(String, Vec<u8>)> = Vec::new();
        for f in &first.copied {
            before.push((f.path.display().to_string(), fs::read(&f.path).expect("read")));
        }
        let second = install(s.path(), out.path()).expect("install again");
        assert_eq!(first.copied.len(), second.copied.len());
        for (p, bytes) in &before {
            assert_eq!(&fs::read(p).expect("read again"), bytes, "{p} changed");
        }
        let n = fs::read_dir(out.path()).unwrap().count();
        assert_eq!(n, ORIGINALS.len() + 1, "the data dir holds the nine files and the marker");
    }

    /// `--data-dir` is the whole layout contract: the files at the top, and `--where` says so.
    #[test]
    fn the_layout_is_what_the_lookup_expects() {
        let Some(s) = copy_of_original("layout") else { return };
        let out = Scratch::new("layout-out");
        install(s.path(), out.path()).expect("install");
        let dirs = resolve(Some(s.path()), Some(out.path())).expect("resolve");
        assert_eq!(dirs.origin, "--data-dir");
        assert_eq!(dirs.originals, s.path());
        assert!(dirs.originals.join("spy.exe").is_file());
    }

    // ---- the lookup order ---------------------------------------------------

    /// 1. `--data-dir` wins over everything, including an installed per-user set.
    #[test]
    fn an_explicit_data_dir_wins() {
        let Some(s) = copy_of_original("win") else { return };
        let out = Scratch::new("win-out");
        install(s.path(), out.path()).expect("install");
        let elsewhere = Scratch::new("win-other");
        let dirs = resolve_with(Some(s.path()), Some(elsewhere.path()), Some(out.path()))
            .expect("resolve");
        assert_eq!(dirs.originals, s.path());
        assert_eq!(dirs.origin, "--data-dir");
    }

    /// 2. an installed per-user set is used when no flag is given.
    #[test]
    fn an_installed_data_dir_is_found_with_no_flags() {
        let Some(s) = copy_of_original("auto") else { return };
        let out = Scratch::new("auto-out");
        install(s.path(), out.path()).expect("install");
        let dirs = resolve_with(None, None, Some(out.path())).expect("resolve");
        assert_eq!(dirs.origin, "installed data dir");
        assert_eq!(dirs.originals, out.path());
    }

    /// 3. a directory that is *not* installed does not shadow the development path: that is what
    ///    keeps every script, demo and test in this checkout working unchanged.
    #[test]
    fn an_empty_data_dir_falls_through_to_the_development_paths() {
        if crate::orig::absent() {
            crate::orig::skip("the development path (no unpacked original here)");
            return;
        }
        let empty = Scratch::new("fallthrough");
        assert!(!is_installed(empty.path()));
        let dirs = resolve_with(None, None, Some(empty.path())).expect("resolve");
        assert_eq!(dirs.origin, "development paths");
        assert!(dirs.originals.ends_with(ORIGINAL_DEV));
    }

    /// **The text a player sees names the data directory and the command, and nothing of ours.**
    ///
    /// A release build must never print `../../spy-vs-spy-iii/original/256`: on the player's
    /// machine that path does not exist, and naming it would send them looking for our checkout.
    /// The command it prints is the one that works where it runs — `./antics` on unix, where a
    /// binary in the current directory is not on `PATH`.
    #[test]
    fn the_not_found_message_tells_the_player_what_to_do() {
        let msg = not_found();
        assert!(msg.starts_with("Original game data not found."), "{msg}");
        assert!(
            msg.contains(&format!("{} --install <folder with your copy", command())),
            "{msg}"
        );
        if cfg!(windows) {
            assert!(!msg.contains("./antics"), "no unix spelling on windows: {msg}");
        } else {
            assert!(msg.contains("Run: ./antics --install"), "the relative form: {msg}");
            // and why that form, once, without guessing at the player's PATH
            assert!(msg.contains("folder you unpacked it in"), "{msg}");
            assert!(msg.contains("PATH"), "{msg}");
        }
        assert!(msg.contains("docs/YOU-NEED-THE-ORIGINAL.md"), "{msg}");
        assert!(!msg.contains("../../"), "the message names a relative path: {msg}");
        assert!(!msg.contains("development"), "{msg}");
        assert!(!msg.contains("assets"), "{msg}");
        // the data directory it did look in is the per-user one
        let d = data_root(None).expect("a home directory");
        assert!(msg.contains(&d.display().to_string()), "{msg}");
    }

    /// **The command in the messages is the one that works there.** On Linux and macOS a binary in
    /// the current directory is not on `PATH`, so a player who followed "`antics --install ~/svs3`"
    /// from the folder they unpacked got `antics: command not found`; `./antics` is what to print.
    /// Windows searches the current directory and ships `antics.exe`.
    #[test]
    fn the_message_prints_the_command_that_works_on_this_platform() {
        if cfg!(windows) {
            assert_eq!(command(), "antics.exe", "the released file name");
        } else {
            assert_eq!(command(), "./antics", "the relative form a shell in that folder runs");
            assert!(!command().starts_with("antics "), "never the bare name on unix");
        }
        // the "not a folder" error carries it too
        let dir = Scratch::new("not-a-folder");
        let bogus = dir.path().join("nope");
        let err = install(&bogus, dir.path()).expect_err("no folder there");
        assert!(err.contains(&format!("{} --install", command())), "{err}");
    }

    /// The data directory is per user and per platform, from `std::env` alone.
    #[test]
    fn the_data_dir_is_per_user() {
        let explicit = PathBuf::from("/tmp/somewhere");
        assert_eq!(data_root(Some(&explicit)), Some(explicit));
        let d = data_root(None).expect("a home directory");
        let s = d.display().to_string();
        assert!(s.contains("antics"), "{s}");
        if cfg!(windows) {
            assert!(s.contains("antics") && !s.contains('/'), "{s}");
        } else if cfg!(target_os = "macos") {
            assert!(s.ends_with("Library/Application Support/antics"), "{s}");
        } else {
            assert!(s.ends_with("antics"), "{s}");
        }
    }
}
