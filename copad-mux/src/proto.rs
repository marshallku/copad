//! The client attach/stream protocol (agent-mux-spec §4b, single-client v1).
//!
//! A persistent [`crate::server`] hosts the authoritative `State` + PTYs; a thin
//! [`crate::client`] renders and forwards input over ndjson on the same Unix socket the `ctl`
//! API uses. A connection's first line selects the role: a `{"cmd":…}` line is a one-shot
//! control request (see [`crate::control`]), a [`ClientMsg::Attach`] opens a streaming session.
//!
//! Render transport is a composed cell diff: the server renders the whole TUI into an
//! off-screen buffer at the client's size and ships only the cells that changed (`Buffer::diff`).

use ratatui::buffer::Cell;
use ratatui::crossterm::event::KeyEvent;
use ratatui::style::{Color, Modifier};
use serde::{Deserialize, Serialize};

/// One rendered cell in a frame: absolute position, glyph and full style.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireCell {
    pub x: u16,
    pub y: u16,
    pub sym: String,
    pub fg: Color,
    pub bg: Color,
    pub mods: Modifier,
    /// Set on the trailing half of a wide glyph; must be reproduced or CJK/emoji break.
    #[serde(default)]
    pub skip: bool,
}

impl WireCell {
    pub fn of(x: u16, y: u16, cell: &Cell) -> Self {
        Self {
            x,
            y,
            sym: cell.symbol().to_string(),
            fg: cell.fg,
            bg: cell.bg,
            mods: cell.modifier,
            skip: cell.skip,
        }
    }

    /// The ONE definition of what a frame does to a client buffer, so the render-fidelity
    /// harnesses model production. These are plain assignments on purpose: `Cell::set_style`
    /// only ever ADDS modifier bits, so BOLD/DIM/REVERSED would stick forever instead of
    /// following the frame.
    pub fn apply_to(&self, dst: &mut Cell) {
        // A control character must not reach the terminal: it moves the cursor instead of filling
        // a cell, so everything printed after it lands in the wrong column. The server no longer
        // sends one, but it outlives client upgrades, so a new client still meets an older server.
        if self.sym.chars().any(char::is_control) {
            dst.set_symbol(" ");
        } else {
            dst.set_symbol(&self.sym);
        }
        dst.fg = self.fg;
        dst.bg = self.bg;
        dst.modifier = self.mods;
        dst.set_skip(self.skip);
    }
}

pub type Rgb8 = (u8, u8, u8);

fn shown() -> bool {
    true
}

/// A render frame: the cells that changed since the client's previous frame, plus the cursor.
/// `full` marks a baseline repaint the client must apply after clearing (attach, resize,
/// takeover). `cols`/`rows` stamp the viewport so a client that just resized can drop
/// stale-size frames until the matching `full` frame arrives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameMsg {
    pub epoch: u64,
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    pub cells: Vec<WireCell>,
    pub cursor: Option<(u16, u16)>,
    #[serde(default = "shown")]
    pub cursor_visible: bool,
    /// The focused program's cursor shape as a `DECSCUSR` parameter; `0` = the terminal's own
    /// default (also what an older server's frame means).
    #[serde(default)]
    pub cursor_shape: u8,
    /// **Silent full repaint.** Re-emit every cell of the client's buffer WITHOUT clearing it
    /// first, the counterpart to `full`, whose `Clear(All)` flashes a blank frame. It recovers a
    /// divergence below the client's buffer: ratatui's cached previous-buffer can drift from
    /// what the terminal shows (its wide-glyph suppression runs over a flat buffer, and an outer
    /// emulator or lossy link can drop a cell), and it then never repaints the stale cell.
    /// Carries no extra cells. See `reconcile_secs`.
    #[serde(default)]
    pub repaint: bool,
    /// The images to show (decisions #194), sent only to a client that said its terminal
    /// draws them ([`ClientMsg::Graphics`]): `None` = unchanged since the last frame this
    /// client got. The client places them over the cells after drawing every frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageRun>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uploads: Vec<ImageUpload>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drops: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRun {
    pub id: u32,
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub src: [u32; 4],
    /// Where in the first cell the image starts, as fractions of a cell (the client turns
    /// them into its own pixels).
    pub off: [f32; 2],
    pub z: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUpload {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Straight (not premultiplied) RGBA8, zlib-compressed, base64 — kitty's `f=32,o=z`.
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replaces {
    pub attachment: u64,
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum ServerMsg {
    /// Sent once, first, on attach: the server-authoritative settings a client needs before it
    /// touches its terminal. `mouse` = whether to enable mouse capture (the server is the single
    /// config authority); `update_environment` = the variable names to refresh from this client
    /// (tmux `update-environment`), answered by [`ClientMsg::Env`] before any input.
    Hello {
        mouse: bool,
        #[serde(default)]
        update_environment: Vec<String>,
        /// The server understands [`ClientMsg::Paste`], so the client may turn on bracketed
        /// paste in its own terminal. A server built before this field never says so — it
        /// would silently drop the unknown message — so the client keeps the old behaviour
        /// there and a paste still arrives as keystrokes.
        #[serde(default)]
        paste: bool,
        /// The server understands [`ClientMsg::Focus`]. Same old-server reasoning as `paste`.
        #[serde(default)]
        focus: bool,
        #[serde(default)]
        colors: bool,
        /// The server process's identity (decisions #166): changes on every restart, and is
        /// the same value the L0 host API's `welcome` carries, so a host holding both a render
        /// and an L0 connection can check they reached the same server. Empty from an older one.
        #[serde(default)]
        incarnation: String,
        #[serde(default)]
        attachment: u64,
        /// The server runs [`ClientMsg::Command`] / answers [`ClientMsg::Ping`] on this
        /// connection (decisions #167). An older server says nothing and would drop both.
        #[serde(default)]
        commands: bool,
        #[serde(default)]
        graphics: bool,
    },
    Reply {
        id: String,
        resp: serde_json::Value,
    },
    Pong {
        id: u64,
    },
    /// The attach was refused before it registered (e.g. `expect_incarnation` did not match:
    /// the server restarted between the host's L0 subscribe and this attach). Nothing about
    /// the shared view changed; the connection closes after this line.
    Refused {
        code: String,
        message: String,
    },
    Frame(FrameMsg),
    /// A pane rang the bell: ring this client's terminal too (tmux `bell-action any`). An older
    /// client cannot parse this and skips it.
    Bell,
    Title {
        text: String,
    },
    /// Put `text` on the system clipboard (the result of a mouse-drag selection copy). The
    /// client emits an OSC 52 sequence to its own terminal — so it works over SSH, unlike a
    /// server-side `pbcopy`/`xclip`. Sent only to the client that made the selection.
    Copy {
        text: String,
    },
    Bye,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MouseKind {
    ScrollUp,
    ScrollDown,
    /// Left button down → focus that pane and anchor a drag-selection there.
    Click,
    Drag,
    /// Left button released → finish the selection; if it moved, copy the pane text.
    Up,
    RightClick,
    RightDrag,
    /// Right button released → over a menu item executes it; elsewhere the menu stays
    /// open for left-click / keyboard selection.
    RightUp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum ClientMsg {
    /// Open a streaming session at the client's terminal size (must be the first line).
    ///
    /// `pid` is only used to find the terminal emulator hosting the client so a notification
    /// jump can raise that window; `serde(default)` keeps older clients compatible.
    Attach {
        cols: u16,
        rows: u16,
        #[serde(default)]
        pid: Option<u32>,
        /// Refuse unless the server's incarnation is this one, BEFORE the attachment
        /// registers (and so before it can resize the shared view). Set by a host that
        /// subscribed first and must not attach to a different, restarted server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_incarnation: Option<String>,
        /// The client pings (decisions #167): the server detaches it after
        /// `HEARTBEAT_EXPIRY` of silence, so a suspended phone stops pinning the shared size.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        heartbeat: bool,
        /// This attach replaces an earlier attachment of the same client (a phone
        /// reconnecting): that one is detached first — but only if its tag matches, so a stale
        /// id cannot evict somebody else.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaces: Option<Replaces>,
    },
    /// The client's values for the variables the server advertised in
    /// [`ServerMsg::Hello::update_environment`], sent right after `Hello` and before any input.
    /// Must stay a struct variant: an internally-tagged enum (`tag = "t"`) cannot serialize a
    /// newtype variant whose payload is a sequence.
    Env {
        vars: Vec<(String, String)>,
        /// The COPAD terminal this client runs inside: `(COPAD_SOCKET, COPAD_PANEL_ID)`, so a
        /// notification jump can name the exact tab. Kept OUT of `vars`: those are re-injected
        /// into every pane comux spawns, and a pane inheriting `COPAD_PANEL_ID` would claim to
        /// be the copad panel.
        #[serde(default)]
        copad_host: Option<(String, String)>,
        /// The binding tag a host UI finds this attachment by in the L0 `clients` topic
        /// (decisions #166): `COPAD_MUX_HOST_TAG`, else `copad:<panel>@<socket>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_tag: Option<String>,
    },
    Key(KeyEvent),
    /// A paste the terminal delivered as ONE bracketed paste, forwarded whole: Claude Code only
    /// attaches a dropped image when its path arrives as a paste. Sent only when `Hello`
    /// advertised `paste`.
    Paste {
        text: String,
    },
    Focus {
        gained: bool,
    },
    /// This client's terminal colours (OSC 10/11), so a pane asking for its background gets the
    /// real answer. Sent only when `Hello` says `colors`.
    Colors {
        fg: Rgb8,
        bg: Rgb8,
    },
    /// This client's terminal draws kitty graphics (probed at attach), and its cell size in
    /// pixels (decisions #194). Sent only when `Hello` said `graphics`.
    Graphics {
        cell_width: u16,
        cell_height: u16,
    },
    Mouse {
        x: u16,
        y: u16,
        kind: MouseKind,
    },
    /// Run a control request (the `{"cmd":…}` object) IN ORDER with this connection's keys
    /// and pastes — so a native client's navigation can never overtake text it already sent
    /// (decisions #167). Answered by [`ServerMsg::Reply`].
    Command {
        id: String,
        req: serde_json::Value,
    },
    Ping {
        id: u64,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Detach,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that predates the fix still sends the '\t' alacritty keeps in a tab's first cell,
    /// and it outlives client upgrades. Printed, that shifts the rest of the row.
    #[test]
    fn a_control_character_from_the_wire_becomes_a_blank() {
        let mut wire = WireCell::of(0, 0, &Cell::new("x"));
        wire.sym = "\t".to_string();
        let mut cell = Cell::new("y");
        wire.apply_to(&mut cell);
        assert_eq!(cell.symbol(), " ");

        wire.sym = "가".to_string();
        wire.apply_to(&mut cell);
        assert_eq!(cell.symbol(), "가");
    }

    /// An `attach` from a client built before the pid field must still open a session.
    #[test]
    fn attach_without_pid_still_parses() {
        let old = r#"{"t":"attach","cols":80,"rows":24}"#;
        match serde_json::from_str::<ClientMsg>(old).expect("old attach must parse") {
            ClientMsg::Attach {
                cols, rows, pid, ..
            } => {
                assert_eq!((cols, rows), (80, 24));
                assert_eq!(pid, None);
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    /// `ClientMsg::Env` MUST be a struct variant: an internally-tagged enum cannot tag a newtype
    /// variant carrying a sequence, so `Env(Vec<…>)` would never serialize.
    #[test]
    fn client_env_round_trips() {
        let msg = ClientMsg::Env {
            vars: vec![
                ("DISPLAY".to_string(), ":0".to_string()),
                ("SSH_CONNECTION".to_string(), "1.2.3.4 5 6".to_string()),
            ],
            copad_host: Some(("/run/copad/gui-9.sock".into(), "panel-7".into())),
            host_tag: None,
        };
        let line = serde_json::to_string(&msg).expect("Env must serialize");
        assert!(line.contains("\"t\":\"env\""));
        match serde_json::from_str::<ClientMsg>(&line).expect("Env must deserialize") {
            ClientMsg::Env {
                vars, copad_host, ..
            } => {
                assert_eq!(vars.len(), 2);
                assert_eq!(vars[0], ("DISPLAY".to_string(), ":0".to_string()));
                assert_eq!(
                    copad_host,
                    Some(("/run/copad/gui-9.sock".into(), "panel-7".into()))
                );
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn right_mouse_kinds_round_trip() {
        for (kind, wire) in [
            (MouseKind::RightClick, "right-click"),
            (MouseKind::RightDrag, "right-drag"),
            (MouseKind::RightUp, "right-up"),
        ] {
            let msg = ClientMsg::Mouse { x: 3, y: 7, kind };
            let line = serde_json::to_string(&msg).unwrap();
            assert!(line.contains(&format!("\"{wire}\"")), "wire form: {line}");
            match serde_json::from_str::<ClientMsg>(&line).unwrap() {
                ClientMsg::Mouse { x: 3, y: 7, .. } => {}
                other => panic!("wrong round-trip: {other:?}"),
            }
        }
    }

    #[test]
    fn hello_without_update_environment_defaults_empty() {
        let m: ServerMsg = serde_json::from_str(r#"{"t":"hello","mouse":true}"#).unwrap();
        match m {
            ServerMsg::Hello {
                mouse,
                update_environment,
                paste,
                focus,
                colors,
                ..
            } => {
                assert!(mouse);
                assert!(update_environment.is_empty());
                assert!(!paste);
                assert!(!focus && !colors);
            }
            _ => panic!("expected Hello"),
        }
    }
}

/// The render wire's JSON grammar, PINNED (decisions #166). A native client (the iOS app)
/// implements exactly these strings; they come from ratatui's and crossterm's serde impls, so a
/// dependency bump that changes one must fail here rather than silently break that client.
#[cfg(test)]
mod wire_grammar {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Modifier};

    fn round<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
        v: T,
        wire: &str,
    ) {
        assert_eq!(serde_json::to_string(&v).unwrap(), wire);
        assert_eq!(serde_json::from_str::<T>(wire).unwrap(), v, "{wire}");
    }

    #[test]
    fn colors() {
        round(Color::Reset, r#""Reset""#);
        round(Color::Red, r#""Red""#);
        round(Color::LightBlue, r#""LightBlue""#);
        round(Color::DarkGray, r#""DarkGray""#);
        round(Color::Rgb(1, 2, 255), r##""#0102FF""##);
        round(Color::Indexed(12), r#""12""#);
    }

    #[test]
    fn modifiers() {
        round(Modifier::empty(), r#""""#);
        round(Modifier::BOLD, r#""BOLD""#);
        round(
            Modifier::BOLD | Modifier::DIM | Modifier::REVERSED,
            r#""BOLD | DIM | REVERSED""#,
        );
        round(
            Modifier::all(),
            r#""BOLD | DIM | ITALIC | UNDERLINED | SLOW_BLINK | RAPID_BLINK | REVERSED | HIDDEN | CROSSED_OUT""#,
        );
    }

    #[test]
    fn keys() {
        let k = |c, m| KeyEvent::new(c, m);
        round(
            k(KeyCode::Char('a'), KeyModifiers::NONE),
            r#"{"code":{"Char":"a"},"modifiers":"","kind":"Press","state":""}"#,
        );
        round(
            k(KeyCode::Char('b'), KeyModifiers::CONTROL),
            r#"{"code":{"Char":"b"},"modifiers":"CONTROL","kind":"Press","state":""}"#,
        );
        round(
            k(KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT),
            r#"{"code":"Enter","modifiers":"SHIFT | ALT","kind":"Press","state":""}"#,
        );
        round(
            k(KeyCode::F(5), KeyModifiers::NONE),
            r#"{"code":{"F":5},"modifiers":"","kind":"Press","state":""}"#,
        );
        for (code, name) in [
            (KeyCode::Up, "Up"),
            (KeyCode::Esc, "Esc"),
            (KeyCode::Backspace, "Backspace"),
            (KeyCode::Tab, "Tab"),
            (KeyCode::Left, "Left"),
            (KeyCode::PageUp, "PageUp"),
        ] {
            round(
                k(code, KeyModifiers::NONE),
                &format!(r#"{{"code":"{name}","modifiers":"","kind":"Press","state":""}}"#),
            );
        }
        round(
            k(KeyCode::BackTab, KeyModifiers::SHIFT),
            r#"{"code":"BackTab","modifiers":"SHIFT","kind":"Press","state":""}"#,
        );
        round(
            k(KeyCode::Char('한'), KeyModifiers::NONE),
            r#"{"code":{"Char":"한"},"modifiers":"","kind":"Press","state":""}"#,
        );
    }
}
