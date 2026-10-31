//! **The console and the message box — Windows only** (v0.1.3).
//!
//! `antics` is a console-subsystem binary (see `ci/release.yml`: the Windows build is
//! `x86_64-pc-windows-msvc`, no `#![windows_subsystem = "windows"]`), so a player who starts it
//! from a Start Menu shortcut or a double-click gets a console window of its own: a black box
//! beside the game while it runs, and — because the process exits as soon as it finishes — a
//! window that flashes and disappears, taking with it the one sentence that said what went wrong.
//! This module is the whole of the fix, and it is four small functions:
//!
//! * [`launched_from_gui`] — *is this console ours alone?* `GetConsoleProcessList` returns the
//!   processes attached to the console this process owns. A shortcut launch attaches exactly one
//!   (this one); a launch from `cmd`, PowerShell or Windows Terminal attaches the shell too, so the
//!   count is 2 or more and the run is left completely alone.
//! * [`streams_redirected`] — an explicit `antics > log.txt` from anywhere is a run that wants its
//!   output in a file, not in a message box.
//! * [`detach_console`] — `FreeConsole`, called once, just before the window opens, so no console
//!   box is left standing beside the game.
//! * [`message_box`] — `MessageBoxW`, an error or an explanation with an OK button, for a fatal
//!   start-up failure that would otherwise be printed to a console nobody is reading.
//!
//! **Nothing here decides anything.** The decision — detach and box, or do neither — is
//! `gui_launch_plan` in `src/main.rs`, a pure function with unit tests that run on every platform
//! (`launch_tests::a_gui_launch_detaches_...`). This module only carries out that decision, so
//! what can be tested on Linux is tested on Linux, and what cannot be compiled here is as small
//! and as literal as it can be.
//!
//! **Every signature below is copied from the Win32 documentation**, with the C types spelled the
//! way the platform ABI needs them (`BOOL` = `i32`, `DWORD` = `u32`, `HANDLE`/`HWND` = `*mut
//! c_void`, `LPCWSTR` = `*const u16`, `UINT` = `u32`). `kernel32` and `user32` are linked
//! explicitly even though the MSVC target imports them by default: the `#[link]` attributes are
//! what make the intent legible, and they cost nothing. There is no `windows-sys`/`winapi`
//! dependency — the six symbols used here are declared by hand, so the crate still builds offline.

#![allow(clippy::upper_case_acronyms)] // the names are Win32's, character for character

use std::ffi::c_void;

// ---------------------------------------------------------------------------------------------
// The C types, as the ABI has them
// ---------------------------------------------------------------------------------------------

/// `BOOL` — a 32-bit signed integer, **not** Rust's `bool` (only its low bit is meaningful).
type Bool = i32;
/// `DWORD` — a 32-bit unsigned integer.
type Dword = u32;
/// `UINT` — a 32-bit unsigned integer.
type Uint = u32;
/// `HANDLE` and `HWND` — opaque pointer-sized handles.
type Handle = *mut c_void;
type Hwnd = *mut c_void;
/// `LPCWSTR` — a pointer to a NUL-terminated UTF-16 string.
type Lpcwstr = *const u16;

// ---------------------------------------------------------------------------------------------
// The constants (winbase.h / winuser.h)
// ---------------------------------------------------------------------------------------------

/// `STD_OUTPUT_HANDLE` — `(DWORD)-11`, the pseudo-handle `GetStdHandle` maps to stdout.
const STD_OUTPUT_HANDLE: Dword = -11i32 as Dword;
/// `STD_ERROR_HANDLE` — `(DWORD)-12`.
const STD_ERROR_HANDLE: Dword = -12i32 as Dword;
/// `FILE_TYPE_CHAR` — a character device; for a console process, the console.
const FILE_TYPE_CHAR: Dword = 0x0000_0002;

/// `MB_OK` — one button, "OK".
const MB_OK: Uint = 0x0000_0000;
/// `MB_ICONERROR` — the red circle with the white cross.
const MB_ICONERROR: Uint = 0x0000_0010;
/// `MB_ICONINFORMATION` — the blue "i".
const MB_ICONINFORMATION: Uint = 0x0000_0040;

// ---------------------------------------------------------------------------------------------
// The imports: six functions, declared by hand
// ---------------------------------------------------------------------------------------------

#[link(name = "kernel32")]
extern "system" {
    /// `DWORD GetConsoleProcessList(DWORD *lpdwProcessList, DWORD dwProcessCount)`
    ///
    /// Fills `lp_process_list` with the process IDs attached to this process's console and returns
    /// how many there are — which is more than `dw_process_count` when the list did not fit, and
    /// `0` when the process has no console at all.
    fn GetConsoleProcessList(lp_process_list: *mut Dword, dw_process_count: Dword) -> Dword;

    /// `BOOL FreeConsole(void)` — detaches this process from its console. Non-zero on success.
    fn FreeConsole() -> Bool;

    /// `HANDLE GetStdHandle(DWORD nStdHandle)` — the standard device for one of the three
    /// selectors, `INVALID_HANDLE_VALUE` (`(HANDLE)-1`) or `NULL` when there is none.
    fn GetStdHandle(n_std_handle: Dword) -> Handle;

    /// `DWORD GetFileType(HANDLE hFile)` — `FILE_TYPE_CHAR` (2) for a console, `FILE_TYPE_PIPE`
    /// (3) for a pipe, `FILE_TYPE_DISK` (1) for a file, `FILE_TYPE_UNKNOWN` (0) on an error.
    fn GetFileType(h_file: Handle) -> Dword;
}

#[link(name = "user32")]
extern "system" {
    /// `int MessageBoxW(HWND hWnd, LPCWSTR lpText, LPCWSTR lpCaption, UINT uType)`
    ///
    /// A modal message box with the given text and caption; returns which button was pressed
    /// (`IDOK` = 1). A null owner window is allowed and is what a box with no parent wants.
    fn MessageBoxW(h_wnd: Hwnd, lp_text: Lpcwstr, lp_caption: Lpcwstr, u_type: Uint) -> i32;
}

// ---------------------------------------------------------------------------------------------
// The four questions and actions
// ---------------------------------------------------------------------------------------------

/// **Is this process the only one attached to its console?**
///
/// True for a double-click and for a Start Menu or desktop shortcut: Windows gives the process a
/// console of its own, so `GetConsoleProcessList` reports exactly one process. False when the
/// process shares the console with the shell it was started from — `cmd.exe`, PowerShell, Windows
/// Terminal, or a `.bat` wrapper — because then the console is the user's and the program must not
/// touch it.
///
/// A small buffer is enough for the question. When the console holds more processes than the four
/// the buffer holds, the call returns the number it *would* need — still not 1, so the answer is the
/// same, and the list does not need to be read.
pub fn launched_from_gui() -> bool {
    let mut pids = [0 as Dword; 4];
    // SAFETY: `pids` is a valid, writable, 4-element array of DWORDs and its length is passed
    // alongside it, which is the whole contract of GetConsoleProcessList. The call only writes into
    // that buffer and returns a DWORD; it allocates nothing and keeps no pointer.
    let attached = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as Dword) };
    attached == 1
}

/// **Do stdout or stderr go somewhere other than the console?**
///
/// `antics > log.txt` (or a launcher that captures the streams) is a run whose output is being
/// read; it is never treated as a GUI launch, whatever the console looks like.
pub fn streams_redirected() -> bool {
    !is_console_handle(STD_OUTPUT_HANDLE) || !is_console_handle(STD_ERROR_HANDLE)
}

/// One standard stream: is it a console? `INVALID_HANDLE_VALUE` is `(HANDLE)-1`, `NULL` is 0, and
/// neither is a console, so both answer "no".
fn is_console_handle(which: Dword) -> bool {
    // SAFETY: GetStdHandle takes a DWORD selector only, and GetFileType only reads the handle it is
    // given. No pointer crosses either call, and both are documented to be safe to call from any
    // thread of a console process.
    unsafe {
        let handle = GetStdHandle(which);
        let raw = handle as isize;
        if raw == 0 || raw == -1 {
            return false;
        }
        GetFileType(handle) == FILE_TYPE_CHAR
    }
}

/// **Drop the console.** Called once, just before the window opens, when
/// `gui_launch_plan(...).detach_before_window` is true.
///
/// The return value is ignored on purpose: a process with no console (already detached, or started
/// without one) reports failure, and that is not a problem for anything downstream — the game does
/// not need a console, and every fatal message from here on goes to a message box as well.
pub fn detach_console() {
    // SAFETY: FreeConsole takes no arguments, allocates nothing and keeps no pointer to this
    // process's memory; it only unlinks the process from its console.
    let _ = unsafe { FreeConsole() };
}

/// **Say something where a player will see it.** An error or an explanation with an OK button, in a
/// box that is modal to nothing (a null owner) and centred by Windows.
///
/// `error` chooses the icon: `MB_ICONERROR` for a fatal start-up failure, `MB_ICONINFORMATION`
/// otherwise. The text and the caption are converted to NUL-terminated UTF-16 here and live until
/// `MessageBoxW` returns — it copies what it needs before it does.
pub fn message_box(title: &str, text: &str, error: bool) {
    let text = wide(text);
    let title = wide(title);
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    // SAFETY: `text` and `title` are NUL-terminated UTF-16 buffers owned by this stack frame, alive
    // across the whole call; a null HWND is the documented "no owner" form. MessageBoxW returns
    // when the user dismisses the box and keeps no pointer afterwards.
    let _ = unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | icon)
    };
}

/// `&str` -> a NUL-terminated UTF-16 buffer, the only string form the `W` APIs take.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
