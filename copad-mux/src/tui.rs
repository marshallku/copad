//! The single-pane TUI: a ratatui front-end over one `PaneTerm`. Enters raw mode
//! and the alternate screen, renders the hosted terminal's grid every tick, and
//! forwards keystrokes to the PTY. Quit with the tmux-like prefix `Ctrl-b` then
//! `q`, or when the shell exits.
//!
//! Work-unit 2: one pane, one process. Splits, the sidebar, the server/client
//! split, and the CLI come in later units.

use std::io::{self, Stdout};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};

use crate::term::{CellColor, PaneTerm};

/// Restores the host terminal (raw mode off + leave alt screen) on drop — so a
/// panic mid-render never leaves the user's terminal wedged.
struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Construct the guard immediately so that if EnterAlternateScreen below
        // fails, its Drop still disables raw mode (no wedged terminal).
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(guard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Run the single-pane TUI to completion. Returns when the user quits (Ctrl-b q)
/// or the hosted shell exits.
pub fn run() -> io::Result<()> {
    // Route panics through the guard-restore too, so a crash restores the terminal.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let size = terminal.size()?;
    let (mut cols, mut rows) = (size.width, size.height);
    let Some(pane) = PaneTerm::spawn(cols, rows, None, None) else {
        return Err(io::Error::other("failed to spawn shell PTY"));
    };

    let mut prefix = false;
    loop {
        if pane.has_exited() {
            break;
        }

        let snap = pane.snapshot();
        terminal.draw(|frame| {
            let area = frame.area();
            let buf = frame.buffer_mut();
            for (y, row) in snap.cells.iter().enumerate() {
                if y as u16 >= area.height {
                    break;
                }
                for (x, cell) in row.iter().enumerate() {
                    if x as u16 >= area.width {
                        break;
                    }
                    if let Some(bc) = buf.cell_mut(Position::new(x as u16, y as u16)) {
                        // The trailing half of a wide grapheme: skip it so ratatui
                        // renders the wide symbol (set in the previous cell) across
                        // both columns instead of overwriting its second half.
                        if cell.spacer {
                            bc.set_skip(true);
                            continue;
                        }
                        let mut style =
                            Style::default().fg(to_color(cell.fg)).bg(to_color(cell.bg));
                        if cell.bold {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if cell.reverse {
                            style = style.add_modifier(Modifier::REVERSED);
                        }
                        bc.set_symbol(&cell.sym);
                        bc.set_style(style);
                    }
                }
            }
            let (cx, cy) = snap.cursor;
            if cx < area.width && cy < area.height {
                frame.set_cursor_position(Position::new(cx, cy));
            }
        })?;

        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        match event::read()? {
            CEvent::Key(k) if k.kind != KeyEventKind::Release => {
                // Prefix mode: Ctrl-b then q quits; any other key after the
                // prefix is swallowed (tmux-like — it's a command slot).
                if prefix {
                    prefix = false;
                    if matches!(k.code, KeyCode::Char('q')) {
                        break;
                    }
                    continue;
                }
                if k.code == KeyCode::Char('b') && k.modifiers.contains(KeyModifiers::CONTROL) {
                    prefix = true;
                    continue;
                }
                if let Some(bytes) = key_to_bytes(k.code, k.modifiers) {
                    pane.input(&bytes);
                }
            }
            CEvent::Resize(w, h) => {
                cols = w;
                rows = h;
                pane.resize(cols, rows);
            }
            _ => {}
        }
    }

    Ok(())
}

fn to_color(c: CellColor) -> Color {
    match c {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(i) => Color::Indexed(i),
        CellColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Translate a key event into the bytes a PTY expects.
fn key_to_bytes(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let esc = |mut v: Vec<u8>| {
        if alt {
            v.insert(0, 0x1b);
        }
        v
    };

    let bytes = match code {
        KeyCode::Char(ch) => {
            let utf8 = |ch: char| {
                let mut s = [0u8; 4];
                ch.encode_utf8(&mut s).as_bytes().to_vec()
            };
            if ctrl {
                // Only the ASCII control combinations map to control bytes;
                // Ctrl with digits / non-ASCII passes the char through unchanged.
                match ch {
                    ' ' | '@' => vec![0], // Ctrl-Space / Ctrl-@ → NUL
                    'a'..='z' | 'A'..='Z' => vec![(ch.to_ascii_lowercase() as u8) & 0x1f],
                    '[' | '\\' | ']' | '^' | '_' => vec![(ch as u8) & 0x1f],
                    _ => utf8(ch),
                }
            } else {
                utf8(ch)
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        _ => return None,
    };
    Some(esc(bytes))
}
