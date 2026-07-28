//! The client attach/stream protocol (agent-mux-spec §4b, single-client v1).
//!
//! A persistent [`crate::server`] hosts the authoritative `State` + PTYs; a thin
//! [`crate::client`] renders and forwards input. They speak ndjson over the same
//! Unix socket the `ctl` API uses — a connection's first line selects the role:
//! a `{"cmd":…}` line is a one-shot control request (see [`crate::control`]),
//! a [`ClientMsg::Attach`] (`{"t":"attach",…}`) opens a streaming session.
//!
//! Render transport is a **composed cell diff**: the server renders the whole TUI
//! into an off-screen buffer at the attached client's size, then ships only the
//! cells that changed since the client's last frame (`Buffer::diff`). This is a
//! deliberate v1 simplification of the spec's semantic-grid directive — it couples
//! composition to one client, which is fine while at most one client attaches at a
//! time; multi-client composition is a later unit (see decisions.md).

use ratatui::crossterm::event::KeyEvent;
use ratatui::style::{Color, Modifier};
use serde::{Deserialize, Serialize};

/// One rendered cell in a frame: absolute position + glyph + full style. Carries
/// the complete ratatui style relevant to rendering (fg/bg + every `Modifier`,
/// incl. `DIM` on inactive panes and `REVERSED`), so the client reproduces the
/// server's composition exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireCell {
    pub x: u16,
    pub y: u16,
    pub sym: String,
    pub fg: Color,
    pub bg: Color,
    /// `ratatui::style::Modifier` bits (BOLD/DIM/REVERSED/…).
    pub mods: Modifier,
    /// The ratatui `skip` flag — set on the trailing half of a wide glyph so the
    /// renderer doesn't clobber it. Must be reproduced or CJK/emoji break.
    #[serde(default)]
    pub skip: bool,
}

/// A render frame: the cells that changed since the client's previous frame, plus
/// the cursor. `full` marks a baseline repaint (diff against an empty buffer) that
/// the client must apply after clearing — emitted on attach, resize, and takeover.
/// `cols`/`rows` stamp the frame's viewport so a client that just resized can drop
/// stale-size frames until the matching `full` frame arrives. `epoch` increments on
/// every baseline reset (attach/resize/takeover) for debugging + future multi-client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameMsg {
    pub epoch: u64,
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    pub cells: Vec<WireCell>,
    pub cursor: Option<(u16, u16)>,
}

/// Server → client messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum ServerMsg {
    /// Sent once, first, on attach: the server-authoritative settings a client needs
    /// before it touches its terminal. `mouse` = whether to enable mouse capture (the
    /// server is the single config authority, so all clients agree regardless of what
    /// each one's local `mux.toml` says). `update_environment` = the variable names the
    /// server wants refreshed from this client (tmux `update-environment`); the client
    /// replies with a [`ClientMsg::Env`] carrying just those it holds, BEFORE any input.
    Hello {
        mouse: bool,
        #[serde(default)]
        update_environment: Vec<String>,
    },
    /// A render frame (delta or full).
    Frame(FrameMsg),
    /// Put `text` on the system clipboard (the result of a mouse-drag selection copy). The
    /// client emits an OSC 52 sequence to its own terminal — so it works over SSH, unlike a
    /// server-side `pbcopy`/`xclip`. Sent only to the client that made the selection.
    Copy { text: String },
    /// Detach acknowledged / forced (Ctrl-b d, takeover, or server shutdown): the
    /// client restores its terminal and exits; the server keeps running.
    Bye,
}

/// A forwarded mouse action. The server maps `(x, y)` (frame cells) to a pane.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MouseKind {
    /// Wheel up → scroll that pane back into history.
    ScrollUp,
    /// Wheel down → scroll that pane toward the live bottom.
    ScrollDown,
    /// Left button down → focus that pane and anchor a drag-selection there.
    Click,
    /// Left button held + moved → extend the drag-selection (clamped to the origin pane).
    Drag,
    /// Left button released → finish the selection; if it moved, copy the pane text.
    Up,
}

/// Client → server messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum ClientMsg {
    /// Open a streaming session at the client's terminal size (must be the first
    /// line on the connection).
    Attach { cols: u16, rows: u16 },
    /// The client's values for the variables the server advertised in
    /// [`ServerMsg::Hello::update_environment`] (only those the client actually has,
    /// valid UTF-8). Sent immediately after `Hello`, before any input, so new panes
    /// this client spawns inherit its live SSH/display session (tmux `update-environment`).
    /// A struct variant (named `vars`) — an internally-tagged enum (`tag = "t"`) cannot
    /// serialize a newtype variant whose payload is a sequence, so this must NOT be `Env(Vec<…>)`.
    Env { vars: Vec<(String, String)> },
    /// A forwarded key event (interpreted server-side: prefix/nav/tabs/input).
    Key(KeyEvent),
    /// A forwarded mouse action at frame cell `(x, y)`.
    Mouse { x: u16, y: u16, kind: MouseKind },
    /// The client's terminal was resized.
    Resize { cols: u16, rows: u16 },
    /// Explicit detach request (a dropped connection detaches too).
    Detach,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ClientMsg::Env` MUST be a struct variant: an internally-tagged enum can't tag a
    /// newtype variant carrying a sequence, so `Env(Vec<…>)` would fail to serialize and
    /// the client's environment handshake would silently never send. Lock the round-trip.
    #[test]
    fn client_env_round_trips() {
        let msg = ClientMsg::Env {
            vars: vec![
                ("DISPLAY".to_string(), ":0".to_string()),
                ("SSH_CONNECTION".to_string(), "1.2.3.4 5 6".to_string()),
            ],
        };
        let line = serde_json::to_string(&msg).expect("Env must serialize");
        assert!(line.contains("\"t\":\"env\""));
        match serde_json::from_str::<ClientMsg>(&line).expect("Env must deserialize") {
            ClientMsg::Env { vars } => {
                assert_eq!(vars.len(), 2);
                assert_eq!(vars[0], ("DISPLAY".to_string(), ":0".to_string()));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// A `Hello` written by an older server (no `update_environment`) must still parse
    /// (the field defaults to empty) so the client degrades to "no refresh" rather than erroring.
    #[test]
    fn hello_without_update_environment_defaults_empty() {
        let m: ServerMsg = serde_json::from_str(r#"{"t":"hello","mouse":true}"#).unwrap();
        match m {
            ServerMsg::Hello {
                mouse,
                update_environment,
            } => {
                assert!(mouse);
                assert!(update_environment.is_empty());
            }
            _ => panic!("expected Hello"),
        }
    }
}
