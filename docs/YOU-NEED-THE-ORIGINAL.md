# You need the original game

`antics` is an unofficial, non-commercial fan reimplementation of the **engine** of
*Spy vs. Spy III: Arctic Antics* — the 1987 MS-DOS game (the files in the supported release are
dated 1987–1988).

This repository contains **no original game code, no art, no sound data and no other assets**. Not in
the source tree, not in the releases. What it contains is Rust code that reimplements the game's
rules, its frame timing, its drawing routines and its screen layout, plus documentation of how that
was measured.

To actually run it you must supply your own copy of the original game. `antics --install` **copies
nine files** out of that copy into a per-user data directory, and from then on the engine draws
everything itself, at run time, from those nine files: a port of the original's own drawing code
(the intro's script machine from `spytitl.exe`, the selector's draw list and the round's console
chrome from `spy.exe`) reads its tables and pictures and puts them on the screen. Nothing is
extracted, converted or pre-rendered, and the port ships no picture of its own.

---

## The files it needs

| file | size (bytes) | sha256 |
|---|---|---|
| `spy.exe` | 111923 | `889d68beee3434047373e21efe8b5148d40ac230836989f10d5546dda4f7889a` |
| `spytitl.exe` | 48914 | `e60cb432379eb8170e7c3c233c8365ad07b7f88b2063e640b22fcb49be0067eb` |
| `TITLE.DAT` | 16000 | `2583590b87b84ea8c39e9e5bae3dc7639a457e5c63964c4d8140f6e496d6a601` |
| `TRAP.PIC` | 16192 | `3a7f0fb94281f4ed882bae7d809cd12e01072e86a2be42aa128d7c04b689736f` |
| `MAXX.PIC` | 16000 | `75ff90f0030483f7b16831b9b498bfddbc58f4e8080e79dbbabf5dc6c870db79` |
| `DEMO.DAT` | 1882 | `2031ff335694f0ddacb81681d25bb3ad146f88ef68b42311327ae120ce18c00d` |
| `TEXT1.DAT` | 4322 | `5df9d6bcd18b276037c18872f56ab12138fc0d2da668dfcf423cf6364bfcb285` |
| `TEXT2.DAT` | 4322 | `be50fafe4747f7cb8a2b2f692af17993275c25346a1df99d6185c2e25396acb8` |
| `TEXT3.DAT` | 4322 | `a7903e5ad2f2c3653f7cc4195c93d4db7e9e71dd0b7494bc25450ad3aa634670` |

All nine are needed: `spy.exe` is the round, the selector and the console chrome; `spytitl.exe` with
`TITLE.DAT`, `MAXX.PIC` and `TEXT1..3.DAT` is the intro; `TRAP.PIC` is the round's and the
selector's screen; `DEMO.DAT` is the attract demo's recorded key stream.

That release is the one this port was built and tested against. In the copy used, `spy.exe` has a
file timestamp of 1988-01-18 and `spytitl.exe` of 1987-12-04; the art is the 256-colour CGA set. A
re-release, a cracked/repacked copy, an Amiga or C64 version or a recompile is **not** supported:
the port only ever ran against the release above, so anything else is refused at install time
rather than decoded into plausible-looking nonsense.

You can check your own copy before installing:

```bash
sha256sum spy.exe spytitl.exe TITLE.DAT TRAP.PIC MAXX.PIC DEMO.DAT TEXT1.DAT TEXT2.DAT TEXT3.DAT      # Linux
shasum -a 256 spy.exe spytitl.exe TITLE.DAT TRAP.PIC MAXX.PIC DEMO.DAT TEXT1.DAT TEXT2.DAT TEXT3.DAT  # macOS
certutil -hashfile spy.exe SHA256                                                                    # Windows, one file at a time
```

You do not have to point `--install` at a folder containing *only* these nine files — the whole
unzipped game directory is fine, and is what you normally have. Upper-case DOS spellings are
accepted too.

## Where to get a copy

This project does not host, mirror, link to or recommend any download. You need to own, or otherwise
be lawfully entitled to use, a copy of *Spy vs. Spy III: Arctic Antics*; how you come by it is your
responsibility alone.

**Copyright on the game still applies.** It is not public domain, it has not been released as
freeware, and nobody has granted permission to redistribute it. Whether you may keep or use a given
copy where you live is for you to determine; this project takes no position on it and gives no legal
advice. A second-hand copy of the original disks is the clean route.

---

## First run, step by step

You need the `antics` binary for your platform and your own unzipped copy of the game.

### Linux

```bash
# 1. unzip your own copy of the game into a folder
mkdir -p ~/svs3 && cd ~/svs3
unzip /path/to/your/spy-vs-spy-3.zip          # or however your copy is packaged
#    you should now see spy.exe, spytitl.exe, TITLE.DAT, TRAP.PIC, MAXX.PIC,
#    DEMO.DAT and TEXT1.DAT..TEXT3.DAT in this folder

# 2. copy those nine files into the port's data directory (one time)
antics --install ~/svs3

# 3. play
antics
```

### Windows (PowerShell)

```powershell
# 1. unzip your own copy
Expand-Archive -Path .\spy-vs-spy-3.zip -DestinationPath .\svs3

# 2. install from it
.\antics.exe --install .\svs3

# 3. play
.\antics.exe
```

### macOS

```bash
# 1. unzip your own copy
mkdir -p ~/svs3 && unzip /path/to/your/spy-vs-spy-3.zip -d ~/svs3

# 2. if you downloaded the binary it is quarantined; clear that first (see Troubleshooting)
xattr -d com.apple.quarantine ./antics

# 3. install, then play
./antics --install ~/svs3
./antics
```

If the data is missing, `antics` says so instead of starting:

```
Original game data not found. Run: antics --install <folder with your copy of Spy vs Spy III: Arctic Antics>. See docs/YOU-NEED-THE-ORIGINAL.md
```

## Where the files land

`--install` **copies** your nine files into a per-user data directory, never next to the binary and
never into the repository. It checks each file's size and SHA-256 first and refuses the whole
folder if one does not match, so a half-copied or wrong-version directory cannot happen:

| OS | default location |
|---|---|
| Linux | `$XDG_DATA_HOME/antics`, i.e. `~/.local/share/antics` when `XDG_DATA_HOME` is unset |
| Windows | `%APPDATA%\antics` (e.g. `C:\Users\you\AppData\Roaming\antics`) |
| macOS | `~/Library/Application Support/antics` |

Nothing outside that directory is written. Deleting the directory is a complete uninstall; re-running
`--install` recreates it.

### `--data-dir DIR`

Points the port at a different data directory for that run, e.g. to keep a copy on an external drive
or to test two installs side by side:

```bash
antics --data-dir /media/usb/antics-data
antics --install ~/svs3 --data-dir /media/usb/antics-data
```

## Troubleshooting

**"the files do not match the version this port supports" / the install step refuses your folder.**
You have a different release. Compare the nine sizes and hashes above. A repack that renames or
strips files, or a version for another platform, will not work — the port needs the MS-DOS
executables, because the code it drives *is* `spy.exe`.

**A file is missing from the data directory.** Run `--install` again pointing at the folder that has
your copy. If one of the nine files is missing from that folder, your copy is incomplete.

**No sound.** This build emits no audio yet: the original's sound cues are located in the code and
documented, but they are not played. A machine with no audio device is therefore not a problem, and
there is nothing to configure. (The game is playable and silent.)

**macOS: "antics cannot be opened because the developer cannot be verified."** The binary is not
signed or notarised (there is no Apple developer account behind this project). Either right-click the
binary in Finder and choose **Open**, then **Open** again in the dialog, or clear the quarantine flag
once:

```bash
xattr -d com.apple.quarantine ./antics
```

If macOS still refuses ("damaged and should be moved to the Trash"), build it yourself from source
with `cargo build --release` — same code, no quarantine.

**Windows: "Windows protected your PC" (SmartScreen).** The binary is unsigned. Click **More info** →
**Run anyway**. If your browser marked the download, right-click the zip → **Properties** →
**Unblock** before extracting.

**The window is tiny / huge.** `--scale N` sets the integer scale (1–8, default 3 = 960×600).

---

## Controls

For the two-player round, as the port maps them (the original's two joystick buttons share the port's
single fire key):

| | move | fire / action |
|---|---|---|
| Player 1 (white spy) | arrow keys | `RETURN` |
| Player 2 (black spy) | `A` `W` `D` `X` (`X` is down) | `Q` |

`Esc` leaves the round, `R` restarts it, `F1` toggles the spy marker, `F2` writes a BMP of the
current frame.

In the setup selector the arrow keys move the selection ring and `RETURN` changes the highlighted
option; on `GO!` it starts the round. On the original the white spy's action key was `Home`; the port
uses `RETURN`.

A tap of fire is the original's button A, holding fire and pressing a direction is its button B
(pick up, put away, dig, bury, open the trapulator, throw a snowball). The full table of what the
fire key does is in the project README.

---

## Not affiliated

This project is not affiliated with, authorised by, sponsored by or endorsed by the rights holders
of *Spy vs. Spy III: Arctic Antics* — Epyx, Tynesoft, First Star Software, or whoever holds those
rights today — nor by DC Comics or MAD Magazine, the owners of the *Spy vs. Spy* property. No
licence, permission or blessing has been sought or received. No original game data is distributed
with it.

*Spy vs. Spy* and all related names, logos and artwork are trademarks or registered trademarks of
their respective owners. They are used here only to describe what this code is a reimplementation
of, in the nominative sense, and no claim is made to them.

If you hold rights in this game and you object to this project, write to the author: the ports and
the source can be taken down. What this project will not do is pretend the downloads were legal.
