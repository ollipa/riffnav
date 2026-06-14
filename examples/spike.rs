//! Phase 0 de-risking spike for riffnav.
//!
//! Proves the two hard parts of the architecture in one binary:
//!   Spike A — read the diff from stdin, then read key events from /dev/tty
//!             (the pager input model). Enabled by crossterm's `use-dev-tty`
//!             feature so events come from the terminal, not the consumed stdin.
//!   Spike B — spawn `delta`, pipe the diff in, capture its ANSI output, and
//!             convert it to a ratatui `Text` via `ansi-to-tui`, then render it
//!             in a scrollable viewport.
//!
//! Run interactively (the real proof — needs a terminal):
//!   cargo run --example spike < .local/sample.diff
//!     j/k or ↑/↓ scroll · Ctrl-d/Ctrl-u half-page · g/G top/bottom · q quit
//!
//! Run headless (validates the delta + ansi-to-tui pipeline without a TTY):
//!   cargo run --example spike -- --dump < .local/sample.diff

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use ansi_to_tui::IntoText;
use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, Paragraph};

fn main() -> Result<()> {
    let dump = std::env::args().any(|a| a == "--dump");

    // Spike A, part 1: the diff arrives on stdin and is fully consumed here.
    let mut diff = String::new();
    std::io::stdin()
        .read_to_string(&mut diff)
        .context("failed to read diff from stdin")?;
    if diff.trim().is_empty() {
        bail!("no diff on stdin — try: cargo run --example spike < .local/sample.diff");
    }

    if dump {
        return dump_mode(&diff);
    }
    run_tui(&diff)
}

/// Spike B: render a diff through delta and convert the ANSI output to a `Text`.
/// This is the function that will graduate into `delta.rs` in Phase 1.
fn render_with_delta(diff: &str, width: u16) -> Result<Text<'static>> {
    let mut child = Command::new("delta")
        .args([
            "--paging=never",
            // delta emits ANSI colors even when its stdout is a pipe (unlike git),
            // so no force-color flag is needed. Force 24-bit so the palette doesn't
            // degrade just because COLORTERM isn't set in our captured environment.
            "--true-color=always",
            "--width",
            &width.to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn `delta` — is it installed and on PATH?")?;

    // Write to delta's stdin on a thread so a large diff can't deadlock against
    // delta filling its stdout pipe while we're still writing.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let owned = diff.to_owned();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(owned.as_bytes());
        // stdin drops here → EOF, so delta knows the input is complete.
    });

    let output = child.wait_with_output().context("waiting on delta")?;
    let _ = writer.join();
    if !output.status.success() {
        bail!(
            "delta exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // ansi-to-tui: bytes with SGR escapes → styled ratatui Text.
    output
        .stdout
        .into_text()
        .context("failed to convert delta's ANSI output to ratatui Text")
}

/// Headless validation: prove the pipeline works without needing a terminal.
fn dump_mode(diff: &str) -> Result<()> {
    let width = 100;
    let text = render_with_delta(diff, width)?;
    let styled_spans: usize = text
        .lines
        .iter()
        .map(|l| l.spans.iter().filter(|s| s.style != Default::default()).count())
        .sum();

    println!("--- spike --dump: delta + ansi-to-tui pipeline ---");
    println!("delta width           : {width}");
    println!("rendered lines         : {}", text.lines.len());
    println!("styled (colored) spans : {styled_spans}");
    println!("--- first 12 lines (plain text reconstruction) ---");
    for line in text.lines.iter().take(12) {
        let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        println!("{plain}");
    }
    if styled_spans == 0 {
        bail!("expected styled spans from delta but found none — color may not be forced");
    }
    println!("--- OK: delta produced colored output and ansi-to-tui parsed it ---");
    Ok(())
}

/// Spike A + B integrated: full-screen scrollable view of the delta-rendered diff,
/// driven by key events read from /dev/tty (not the consumed stdin).
fn run_tui(diff: &str) -> Result<()> {
    // ratatui::init() enters the alternate screen, enables raw mode, and installs
    // a panic hook that restores the terminal on a crash.
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, diff);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, diff: &str) -> Result<()> {
    let mut scroll: u16 = 0;
    let mut last_width: u16 = 0;
    let mut text = Text::default();
    let mut total_lines: u16 = 0;

    loop {
        // Re-render through delta only when the available width changes (resize):
        // this is the cache-invalidation seam the real app will reuse.
        let size = terminal.size()?;
        let inner_width = size.width.saturating_sub(2).max(20); // minus borders
        if inner_width != last_width {
            text = render_with_delta(diff, inner_width)?;
            total_lines = text.lines.len() as u16;
            last_width = inner_width;
        }

        let view_height = terminal.size()?.height.saturating_sub(2);
        let max_scroll = total_lines.saturating_sub(view_height);
        scroll = scroll.min(max_scroll);

        terminal.draw(|frame| {
            let title = format!(
                " riffnav spike — line {}/{}  (j/k scroll · Ctrl-d/u · g/G · q quit) ",
                scroll.saturating_add(1).min(total_lines.max(1)),
                total_lines
            );
            let para = Paragraph::new(text.clone())
                .block(Block::default().borders(Borders::ALL).title(title))
                .scroll((scroll, 0));
            frame.render_widget(para, frame.area());
        })?;

        // Spike A, part 2: these events come from /dev/tty even though stdin was
        // the diff. Without crossterm's `use-dev-tty` feature this would block /
        // read garbage from the consumed pipe.
        let half = view_height / 2;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('c') if ctrl => break,
                KeyCode::Char('j') | KeyCode::Down => scroll = scroll.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
                KeyCode::Char('d') if ctrl => scroll = scroll.saturating_add(half),
                KeyCode::Char('u') if ctrl => scroll = scroll.saturating_sub(half),
                KeyCode::Char('g') => scroll = 0,
                KeyCode::Char('G') => scroll = max_scroll,
                _ => {}
            }
        }
    }
    Ok(())
}
