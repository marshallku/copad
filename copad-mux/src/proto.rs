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
    /// A render frame (delta or full).
    Frame(FrameMsg),
    /// Detach acknowledged / forced (Ctrl-b d, takeover, or server shutdown): the
    /// client restores its terminal and exits; the server keeps running.
    Bye,
}

/// Client → server messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum ClientMsg {
    /// Open a streaming session at the client's terminal size (must be the first
    /// line on the connection).
    Attach { cols: u16, rows: u16 },
    /// A forwarded key event (interpreted server-side: prefix/nav/tabs/input).
    Key(KeyEvent),
    /// The client's terminal was resized.
    Resize { cols: u16, rows: u16 },
    /// Explicit detach request (a dropped connection detaches too).
    Detach,
}
