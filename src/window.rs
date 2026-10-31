//! The SDL2 window: an integer-scaled, nearest-neighbour view of the port's own 320x200 indexed
//! framebuffer (ADR 0001 decided Rust + `sdl2` 0.38 against the system SDL2).
//!
//! This module is deliberately *only* a display and an input device. It owns no game state and no
//! timing decision the engine depends on: `main` asks it for the keys that are down, feeds them to
//! `game::Game::tick`, and hands back a `Screen` to present. That way the windowed path and the
//! scripted `--script` path drive exactly the same engine, and the wall clock exists in exactly one
//! place — here, in `pace()`.
//!
//! The whole module is behind the default-on `window` cargo feature, so a build with
//! `--no-default-features` has no SDL2 dependency at all and every headless path still works.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sdl2::audio::{AudioCallback, AudioDevice, AudioSpecDesired};
use sdl2::event::Event;
use sdl2::keyboard::{Keycode, Scancode};
use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{Canvas, ScaleMode};
use sdl2::EventPump;

use crate::game::Keys;
use crate::Screen;

/// One frame's worth of input, in the shapes the two screens need: `pressed` for the setup
/// selector's discrete key handling (same key names `Selector::key` takes), `p1`/`p2` for the
/// round's held keys.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// Keys that went down during this frame, as `up`/`down`/`left`/`right`/`return`/`escape`/...
    pub pressed: Vec<String>,
    pub p1: Keys,
    pub p2: Keys,
    /// A mouse button went down this frame. The intro takes a click as "skip", the way a player
    /// clicking through a title sequence expects.
    pub click: bool,
    pub quit: bool,
}

impl Frame {
    pub fn is_pressed(&self, name: &str) -> bool {
        self.pressed.iter().any(|p| p == name)
    }
}

pub struct Window {
    canvas: Canvas<sdl2::video::Window>,
    pump: EventPump,
    /// One engine tick: 1/240 s (stage 16a). The tick is the original's **reprogrammed timer**
    /// (`CS:4578`: PIT divisor 4972 -> 239.98 Hz), not a 60 Hz convenience; a game frame is 26 of
    /// them, so the round is redrawn 9.23 times a second while the loop still samples the keyboard
    /// 240 times a second.
    frame_time: Duration,
    next_frame: Instant,
    rgb: Vec<u8>,
    pub scale: u32,
}

impl Window {
    pub fn open(title: &str, scale: u32) -> Result<Window, String> {
        let scale = scale.clamp(1, 8);
        let sdl = sdl2::init().map_err(|e| format!("SDL init: {e}"))?;
        let video = sdl.video().map_err(|e| format!("SDL video: {e}"))?;
        // SDL2's last-resort video driver is `dummy`, which "opens" a window that can never appear.
        // On a box with no display driver at all that is a *silent* failure — the user sees nothing
        // and the process exits 0 — so it is an error here.
        let explicit = std::env::var_os("SDL_VIDEODRIVER").is_some();
        if let Some(cause) = no_display_driver(video.current_video_driver(), explicit) {
            return Err(cause);
        }
        let window = video
            .window(title, crate::W as u32 * scale, crate::H as u32 * scale)
            .position_centered()
            .build()
            .map_err(|e| format!("SDL window: {e}"))?;
        let canvas = window
            .into_canvas()
            .build()
            .map_err(|e| format!("SDL canvas: {e}"))?;
        let pump = sdl.event_pump().map_err(|e| format!("SDL events: {e}"))?;
        Ok(Window {
            canvas,
            pump,
            frame_time: Duration::from_nanos(1_000_000_000 / crate::game::TICK_HZ as u64),
            next_frame: Instant::now(),
            rgb: vec![0u8; crate::PIXELS * 3],
            scale,
        })
    }

    /// Drain the event queue into one frame of input. Held state comes from the keyboard's current
    /// state so a key held down walks continuously; `pressed` carries the edges the selector needs.
    pub fn pump(&mut self) -> Frame {
        let mut f = Frame::default();
        for ev in self.pump.poll_iter() {
            match ev {
                Event::Quit { .. } => f.quit = true,
                Event::MouseButtonDown { .. } => f.click = true,
                Event::KeyDown { keycode: Some(k), repeat: false, .. } => {
                    if let Some(name) = key_name(k) {
                        f.pressed.push(name.to_string());
                    }
                    if k == Keycode::Escape {
                        f.pressed.push("escape".to_string());
                    }
                }
                _ => {}
            }
        }
        let kb = self.pump.keyboard_state();
        // Scancodes, not keycodes: the original reads the keyboard's scancode port (0x60) directly,
        // so binding by scancode is both simpler and closer to it.
        let down = |k: Scancode| kb.is_scancode_pressed(k);
        f.p1 = Keys {
            up: down(Scancode::Up),
            down: down(Scancode::Down),
            left: down(Scancode::Left),
            right: down(Scancode::Right),
            fire: down(Scancode::Return) || down(Scancode::KpEnter),
            // the second button (`DS:0x3283`) has no key: stage 7's one-fire-key mapping makes a
            // held fire plus a direction the B press, and only the computer uses the flag itself
            fire_b: false,
        };
        // P2 is A W D X and Q -- "A W D X, not WASD; X is DOWN" (measured; it cost three rounds of
        // failed probes). S is not a movement key in the original's P2 block.
        f.p2 = Keys {
            up: down(Scancode::W),
            down: down(Scancode::X),
            left: down(Scancode::A),
            right: down(Scancode::D),
            fire: down(Scancode::Q),
            fire_b: false,
        };
        f
    }

    /// Show one 320x200 indexed frame, scaled by whole pixels, in the round's palette.
    pub fn present(&mut self, screen: &Screen) -> Result<(), String> {
        self.present_with(screen, &crate::round::ROUND_PALETTE)
    }

    /// The same with a caller's palette: the intro's MAXX card is CGA **palette 0** (`green/red/
    /// yellow`), while the arctic scene and every round screen are palette 1 (`cyan/magenta/white`).
    /// Presenting the card through the round palette would draw it in the wrong colours.
    pub fn present_with(&mut self, screen: &Screen, palette: &[[u8; 3]; 4]) -> Result<(), String> {
        for (i, &v) in screen.idx.iter().enumerate().take(crate::PIXELS) {
            let c = palette[(v & 3) as usize];
            // SDL's RGB24 really is R, G, B (the B, G, R order is a BMP-file trap, see ADR 0001).
            self.rgb[i * 3] = c[0];
            self.rgb[i * 3 + 1] = c[1];
            self.rgb[i * 3 + 2] = c[2];
        }
        // The texture is created per frame: `Texture` borrows its `TextureCreator`, which cannot be
        // stored next to the canvas it came from without self-referential gymnastics, and one
        // streamed 320x200 texture a frame is cheaper than the alternatives.
        let creator = self.canvas.texture_creator();
        let mut texture = creator
            .create_texture_streaming(PixelFormatEnum::RGB24, crate::W as u32, crate::H as u32)
            .map_err(|e| format!("SDL texture: {e}"))?;
        // whole-pixel scaling only: the probe in ADR 0001 proved a 1-px checkerboard survives with
        // zero blended pixels this way.
        texture.set_scale_mode(ScaleMode::Nearest);
        texture
            .update(None, &self.rgb, crate::W * 3)
            .map_err(|e| format!("texture upload: {e}"))?;
        self.canvas.clear();
        self.canvas
            .copy(&texture, None, None)
            .map_err(|e| format!("copy: {e}"))?;
        self.canvas.present();
        Ok(())
    }

    /// Sleep until the next engine tick (240 a second). This is the only use of the wall clock in
    /// the whole port, and nothing the engine computes depends on it — the engine only counts
    /// ticks. The sleep is 4.17 ms, which the OS honours well enough that the frame's own 26-tick
    /// wait stays within a tick of 108.34 ms (measured on the port: see the stage 16a report).
    pub fn pace(&mut self) {
        self.next_frame += self.frame_time;
        let now = Instant::now();
        if self.next_frame > now {
            std::thread::sleep(self.next_frame - now);
        } else if now - self.next_frame > Duration::from_millis(250) {
            // We fell far behind (a suspended machine, or a slow frame): resynchronise rather than
            // burn a burst of catch-up ticks.
            self.next_frame = now;
        }
    }

    /// Write the frame currently on screen to a BMP (used by `--play --shot-every` so a windowed run
    /// leaves artefacts behind).
    pub fn save(&self, path: &std::path::Path, screen: &Screen) -> Result<(), String> {
        crate::write_bmp(path, screen, &crate::round::ROUND_PALETTE)
    }
}


// ---------------------------------------------------------------- the PC speaker

/// The square wave the original's speaker makes: PIT channel 2 in **mode 3**, clocked by
/// 1.193182 MHz, gated by bit 0 of port 0x61. Nothing else — no filter, no envelope.
///
/// The sequencer runs on the engine's own thread (240 ticks a second, one port write every few
/// ticks); the audio device calls this from its own thread a few hundred samples at a time. The
/// two meet in one atomic word — `divisor << 1 | gate` — which the callback reads once per buffer
/// and holds until the next read. At a 512-sample buffer and 48 kHz that is a 10.7 ms hold, so a
/// note's *pitch* can arrive up to one buffer late; the note *lengths* are the sequencer's own and
/// unaffected. The report (§6) measures this: it is the one place the port's sound is not
/// sample-exact against the recording.
pub struct SquareWave {
    state: Arc<AtomicU32>,
    /// Playback rate the device actually gave us (it may not be the 48 kHz asked for).
    rate: u32,
    phase: f64,
    /// Calibration: the port's wave is a hard square, the real cone is a first-order low-pass, so
    /// at 32 Hz (div 36488) the original is barely a click. Volume is fixed and low.
    volume: f32,
}

impl AudioCallback for SquareWave {
    type Channel = i16;

    fn callback(&mut self, out: &mut [i16]) {
        let word = self.state.load(Ordering::Relaxed);
        let gate = word & 1 != 0;
        let div = (word >> 1) as u16;
        let hz = if div == 0 || !gate { 0.0 } else { crate::sound::hz(div) };
        let inc = hz / self.rate as f64;
        let amp = (self.volume * i16::MAX as f32) as f64;
        for s in out.iter_mut() {
            if hz <= 0.0 {
                *s = 0;
                continue;
            }
            // a square: high for the first half of the cycle. `phase` is kept in [0, 1).
            *s = if self.phase < 0.5 { amp as i16 } else { -(amp as i16) };
            self.phase += inc;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
        }
    }
}

/// The speaker device: open it for a windowed run, or leave it closed and only read the sequencer.
pub struct Speaker {
    /// Nothing reads this, but dropping it closes the device: the speaker must own it.
    #[allow(dead_code)]
    device: AudioDevice<SquareWave>,
    state: Arc<AtomicU32>,
    pub rate: u32,
}

impl Speaker {
    /// Open a mono 48 kHz `i16` device. `None` when SDL has no audio (a headless machine, or
    /// `--no-sound`): the caller then just keeps the sequencer running and silent.
    pub fn open(volume: f32) -> Option<Speaker> {
        let audio = match sdl2::init().ok()?.audio() {
            Ok(a) => a,
            Err(_) => return None,
        };
        let want = AudioSpecDesired { freq: Some(48_000), channels: Some(1), samples: Some(512) };
        let state = Arc::new(AtomicU32::new(0));
        let shared = Arc::clone(&state);
        let device = audio
            .open_playback(None, &want, move |spec| SquareWave {
                state: shared,
                rate: spec.freq.max(1) as u32,
                phase: 0.0,
                volume,
            })
            .ok()?;
        let rate = device.spec().freq as u32;
        device.resume();
        Some(Speaker { device, state, rate })
    }

    /// One sequencer tick's worth of port writes. `div` and `gate` are applied to the same atomic
    /// word, so the callback sees a note's pitch and its gate together.
    pub fn set(&self, out: crate::sound::Out) {
        let mut word = self.state.load(Ordering::Relaxed);
        if let Some(d) = out.div {
            word = ((d as u32) << 1) | (word & 1);
        }
        if let Some(g) = out.gate {
            word = (word & !1) | u32::from(g);
        }
        self.state.store(word, Ordering::Relaxed);
    }

    /// Silence immediately (round end, a restart, Escape): the original turns the speaker off at
    /// `CS:0448`/`CS:051F` when a round ends, so nothing is left droning.
    pub fn silence(&self) {
        self.state.store(0, Ordering::Relaxed);
    }

}

/// **Is the video driver SDL chose one that cannot show a window?**
///
/// `dummy` is SDL2's last resort when no display driver initialises, and `offscreen` is its
/// headless counterpart in newer builds: either one draws nothing, so a run that got one of them is
/// a run the player sees nothing of. Only SDL's *own* choice is an error — `SDL_VIDEODRIVER=dummy`
/// set by the caller is deliberate (the headless smoke tests use it). Returns the cause for the
/// error message, or `None` when the driver can draw.
fn no_display_driver(driver: &str, explicit_driver: bool) -> Option<String> {
    if explicit_driver || !matches!(driver, "dummy" | "offscreen") {
        return None;
    }
    Some(format!(
        "no display driver available: SDL fell back to its `{driver}` driver, which draws nothing"
    ))
}

/// The key names the setup selector's own key handler uses.
fn key_name(k: Keycode) -> Option<&'static str> {
    match k {
        Keycode::Up => Some("up"),
        Keycode::Down => Some("down"),
        Keycode::Left => Some("left"),
        Keycode::Right => Some("right"),
        Keycode::Return | Keycode::KpEnter | Keycode::Space => Some("return"),
        Keycode::Escape => Some("escape"),
        Keycode::R => Some("r"),
        Keycode::F1 => Some("f1"),
        Keycode::F2 => Some("f2"),
        Keycode::P => Some("p"),
        _ => None,
    }
}

// ---- tests ---------------------------------------------------------------
//
// Pure ones only: no SDL, so they run on a machine with no display, which is exactly the case the
// check below is about.
#[cfg(test)]
mod tests {
    use super::no_display_driver;

    /// **Stage 30 — the silent headless exit.** SDL2 reports success when it falls back to its
    /// `dummy` video driver: the window is created and never appears, which is how the owner saw
    /// "nothing happens" on a box with no display. That is now an error that names the driver.
    #[test]
    fn a_dummy_video_fallback_is_an_error_unless_the_caller_asked_for_it() {
        for d in ["dummy", "offscreen"] {
            let m = no_display_driver(d, false).expect("auto-selected headless driver is an error");
            assert!(m.contains(d), "the message names the driver: {m}");
            assert!(m.contains("draws nothing"), "and what is wrong with it: {m}");
            assert!(no_display_driver(d, true).is_none(),
                    "SDL_VIDEODRIVER={d} is the caller's own choice");
        }
        // a real driver is never an error, whatever the environment says
        for d in ["x11", "wayland", "cocoa", "windows", "kmsdrm"] {
            assert!(no_display_driver(d, false).is_none(), "{d} draws");
        }
    }
}
