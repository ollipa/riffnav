//! Terminal modes riffnav turns on and off, and panics on other threads.

use std::sync::{Mutex, PoisonError};
use std::{panic, thread};

use ratatui::DefaultTerminal;

/// Button presses (wheel ticks included) plus motion *while a button is held*,
/// for dragging the divider, with SGR-encoded coordinates (`?1002` + `?1006`).
/// Not crossterm's `EnableMouseCapture`, which also enables any-motion tracking
/// (`?1003`): that would wake the loop to repaint the whole screen for every
/// mouse twitch.
const MOUSE_ON: &[u8] = b"\x1b[?1002h\x1b[?1006h";

const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l";

/// Best-effort: on while the TUI runs, off whenever we hand the terminal back
/// (teardown, or `$EDITOR`). A terminal without mouse support just keeps working
/// off the keyboard.
pub(super) fn enable_mouse() {
    write_stdout(MOUSE_ON);
}

pub(super) fn disable_mouse() {
    write_stdout(MOUSE_OFF);
}

/// Kitty-style key reporting, asked for on the same schedule as mouse reporting.
/// The composer saves on `Enter` and breaks the line on `Shift-Enter`, and a
/// legacy terminal encodes both as a bare carriage return — only the enhanced
/// protocol tells them apart. Terminals that don't answer the query are left
/// alone, where `Alt-Enter` stands in for the new line.
pub(super) fn enable_enhanced_keys() {
    use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};

    if !crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
        return;
    }
    let pushed = crossterm::execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    if pushed.is_ok() {
        ENHANCED_KEYS.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(super) fn disable_enhanced_keys() {
    if ENHANCED_KEYS.swap(false, std::sync::atomic::Ordering::Relaxed) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PopKeyboardEnhancementFlags
        );
    }
}

/// Whether we have a keyboard-enhancement stack entry to pop, so handing the
/// terminal back never pops a mode we never pushed.
static ENHANCED_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn write_stdout(bytes: &[u8]) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Panics on other threads, to report once the terminal is back.
static BACKGROUND_PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `ratatui::init`, keeping the panic hook it installs to the UI thread.
///
/// That hook restores the terminal, which is right for a panic that's about to end
/// the app. On any other thread the app carries on, in a terminal that's no longer
/// set up for it, and `catch_unwind` can't help, as the hook has run by then. This
/// goes on top of ratatui's hook, so it has to follow every `ratatui::init`.
pub(super) fn init_terminal() -> DefaultTerminal {
    let terminal = ratatui::init();
    let ui = thread::current().id();
    let restoring = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if thread::current().id() == ui {
            restoring(info);
        } else {
            let mut panics = BACKGROUND_PANICS
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            panics.push(info.to_string());
        }
    }));
    terminal
}

/// Say what panicked off the UI thread, now there's a terminal to say it on.
pub(super) fn report_background_panics() {
    let mut panics = BACKGROUND_PANICS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    for panic in panics.drain(..) {
        eprintln!("riffnav: {panic}");
    }
}
