//! Images on a VTE terminal (decisions #190). VTE draws no kitty graphics (and Arch's libvte has
//! no sixel), so the relay in front of it keeps a SHADOW alacritty terminal fed with exactly the
//! bytes VTE is given. The shadow runs copad's kitty graphics code (copad-graphics, the same as
//! macOS), so its cells say where every image fragment is — scrolled, erased, on the alternate
//! screen — and the GTK side draws those fragments over (or, for z < 0, under) VTE.
//!
//! Keeping the two terminals in step:
//! - APC bodies never reach VTE; their introducer and terminator (or the byte that cancelled
//!   them) still do, so VTE's parser makes the same transitions. A placement's cursor movement
//!   and scrolling are replayed to VTE as plain sequences (`Effects`).
//! - The shadow is fed only bytes already written to VTE, so it is never ahead of what VTE has.
//! - What VTE has PARSED is learned in band: after publishing image runs the relay writes an
//!   `OSC 6` marker carrying the publication's number; VTE reports the current-file URI it sets,
//!   and the GTK side shows the newest publication VTE has reached. Markers go in only where the
//!   parser is between sequences, and at most a few publications wait for acknowledgement.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::GraphicImage;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use copad_graphics::{CopadGraphic, Effects, Graphics};

/// History the shadow keeps: images further back in VTE's scrollback are not shown.
const SHADOW_HISTORY: usize = 200;
/// Largest APC body kept (the vendored parser's limit); a longer one is dropped.
const MAX_APC: usize = 16 * 1024 * 1024;
/// Publications waiting for VTE to reach their marker; past this the relay waits.
const MAX_OUTSTANDING: u64 = 4;
/// The marker URI's prefix; its last path component is the publication number.
pub const MARKER_PREFIX: &str = "file:///copad-image-sync/";

/// Visible image runs at one point of the stream. Rows count from the top of the shadow's
/// retained history (`history` rows above the screen).
pub struct Published {
    pub seq: u64,
    pub history: usize,
    pub runs: Vec<CopadGraphic>,
    pub images: Vec<Arc<GraphicImage>>,
}

/// Between the relay's output worker and the GTK side.
#[derive(Default)]
pub struct ImageShared {
    /// `[terminal] images` and the cell size, read by the graphics code.
    pub graphics: Arc<copad_graphics::Shared>,
    published: Mutex<VecDeque<Published>>,
    acked: AtomicU64,
}

impl ImageShared {
    /// VTE has parsed through marker `seq`: the newest publication at or before it, dropping
    /// the older ones. `None` when nothing new is due.
    pub fn acknowledge(&self, seq: u64) -> Option<Published> {
        self.acked.fetch_max(seq, Ordering::AcqRel);
        let mut ring = self.published.lock().unwrap();
        let mut newest = None;
        while ring.front().is_some_and(|p| p.seq <= seq) {
            newest = ring.pop_front();
        }
        newest
    }

    fn outstanding(&self, last: u64) -> u64 {
        last.saturating_sub(self.acked.load(Ordering::Acquire))
    }
}

/// The marker's publication number, from VTE's current-file URI.
pub fn marker_seq(uri: &str) -> Option<u64> {
    uri.strip_prefix(MARKER_PREFIX)?.parse().ok()
}

/// What the relay writes to VTE, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Segment {
    /// Shell output: written to VTE, then fed to the shadow.
    Feed(Vec<u8>),
    /// A complete APC body: run through the shadow's graphics when it reaches the front.
    Apc(Vec<u8>),
    /// Written to VTE only (replayed effects, markers).
    Raw(Vec<u8>),
}

/// Splits shell output into what VTE gets and the APC bodies it must not, keeping state across
/// reads. An `ESC _` starts an APC in any state (an ESC ends every string and sequence first),
/// `ESC \` ends it; CAN or SUB, or an ESC followed by anything else, cancel it — the same
/// contract as the vendored parser.
#[derive(Default)]
pub struct ApcSplitter {
    state: SplitState,
    body: Vec<u8>,
    overflow: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum SplitState {
    #[default]
    Ground,
    /// Saw ESC outside an APC.
    Escape,
    Apc,
    /// Saw ESC inside an APC.
    ApcEscape,
}

impl ApcSplitter {
    pub fn split(&mut self, input: &[u8], out: &mut VecDeque<Segment>) {
        let mut pass = Vec::with_capacity(input.len());
        let mut i = 0;
        while i < input.len() {
            // Runs without a byte that changes state are copied whole: plain output is most
            // of what goes through here.
            match self.state {
                SplitState::Ground => {
                    let run = input[i..]
                        .iter()
                        .position(|&b| b == 0x1b)
                        .unwrap_or(input.len() - i);
                    pass.extend_from_slice(&input[i..i + run]);
                    i += run;
                    if i == input.len() {
                        break;
                    }
                }
                SplitState::Apc => {
                    let run = input[i..]
                        .iter()
                        .position(|&b| matches!(b, 0x1b | 0x18 | 0x1a))
                        .unwrap_or(input.len() - i);
                    let room = MAX_APC.saturating_sub(self.body.len());
                    self.body.extend_from_slice(&input[i..i + run.min(room)]);
                    if run > room {
                        self.overflow = true;
                    }
                    i += run;
                    if i == input.len() {
                        break;
                    }
                }
                _ => {}
            }
            let b = input[i];
            i += 1;
            match self.state {
                SplitState::Ground => {
                    if b == 0x1b {
                        self.state = SplitState::Escape;
                    }
                    pass.push(b);
                }
                SplitState::Escape => {
                    pass.push(b);
                    self.state = match b {
                        b'_' => {
                            self.body.clear();
                            self.overflow = false;
                            SplitState::Apc
                        }
                        0x1b => SplitState::Escape,
                        _ => SplitState::Ground,
                    };
                }
                SplitState::Apc => match b {
                    0x1b => self.state = SplitState::ApcEscape,
                    0x18 | 0x1a => {
                        pass.push(b);
                        self.state = SplitState::Ground;
                    }
                    _ if self.body.len() >= MAX_APC => self.overflow = true,
                    _ => self.body.push(b),
                },
                SplitState::ApcEscape => {
                    if b == b'\\' {
                        // The empty APC VTE sees ends here; the body runs at this point.
                        pass.extend_from_slice(b"\x1b\\");
                        if !pass.is_empty() {
                            out.push_back(Segment::Feed(std::mem::take(&mut pass)));
                        }
                        if !self.overflow {
                            out.push_back(Segment::Apc(std::mem::take(&mut self.body)));
                        }
                        self.body = Vec::new();
                        self.state = SplitState::Ground;
                    } else {
                        // Cancelled: that ESC starts the next sequence.
                        pass.push(0x1b);
                        pass.push(b);
                        self.state = if b == b'_' {
                            self.body.clear();
                            self.overflow = false;
                            SplitState::Apc
                        } else if b == 0x1b {
                            SplitState::Escape
                        } else {
                            SplitState::Ground
                        };
                    }
                }
            }
        }
        if !pass.is_empty() {
            out.push_back(Segment::Feed(pass));
        }
    }
}

/// The shadow drops every event: VTE answers the program itself.
#[derive(Clone, Copy)]
struct Quiet;

impl EventListener for Quiet {
    fn send_event(&self, _: Event) {}
}

pub struct Shadow {
    term: Term<Quiet>,
    parser: Processor,
    graphics: Graphics,
    shared: Arc<ImageShared>,
    replies: Arc<Mutex<Vec<u8>>>,
    seq: u64,
    published_revision: u64,
    published_any: bool,
    fed: bool,
}

impl Shadow {
    pub fn new(columns: usize, lines: usize, shared: Arc<ImageShared>) -> Self {
        let config = Config {
            scrolling_history: SHADOW_HISTORY,
            kitty_keyboard: false,
            ..Config::default()
        };
        let term = Term::new(config, &TermSize::new(columns.max(1), lines.max(1)), Quiet);
        let replies = Arc::new(Mutex::new(Vec::new()));
        let sink = replies.clone();
        let graphics = Graphics::new(
            shared.graphics.clone(),
            Box::new(move |r: String| sink.lock().unwrap().extend_from_slice(r.as_bytes())),
        );
        Self {
            term,
            parser: Processor::new(),
            graphics,
            shared,
            replies,
            seq: 0,
            published_revision: 0,
            published_any: false,
            fed: false,
        }
    }

    pub fn resize(&mut self, columns: usize, lines: usize) {
        if (self.term.columns(), self.term.screen_lines()) != (columns, lines)
            && columns > 0
            && lines > 0
        {
            self.term.resize(TermSize::new(columns, lines));
            self.fed = true;
        }
    }

    /// Bytes VTE was just given.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        // The shadow never renders, so it never holds a synchronized update back.
        if self.parser.sync_bytes_count() > 0 {
            self.parser.stop_sync(&mut self.term);
        }
        self.fed = true;
    }

    /// Run one graphics command; returns the bytes that make VTE do what it did to the cursor.
    pub fn run_apc(&mut self, body: &[u8]) -> Vec<u8> {
        // (The APC's empty stand-in already went through `feed`.)
        let effects = self.graphics.handle(&mut self.term, body);
        self.effect_bytes(effects)
    }

    /// Graphics replies for the program, oldest first.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.replies.lock().unwrap())
    }

    fn effect_bytes(&self, effects: Effects) -> Vec<u8> {
        let mut out = Vec::new();
        if effects.scrolled > 0 {
            out.extend_from_slice(format!("\x1b[{}S", effects.scrolled).as_bytes());
        }
        if let Some(p) = effects.cursor {
            // CUP counts from the scroll region's top in origin mode.
            let top = if self.term.mode().contains(TermMode::ORIGIN) {
                self.term.scroll_region().start.0
            } else {
                0
            };
            out.extend_from_slice(
                format!("\x1b[{};{}H", p.line.0 - top + 1, p.column.0 + 1).as_bytes(),
            );
        }
        if effects.linefeed {
            out.push(b'\n');
        }
        out
    }

    /// When the images on show may have changed and a marker can go in here, publish the
    /// visible runs and return the marker to write next.
    pub fn publish(&mut self) -> Option<Vec<u8>> {
        let revision = self.graphics.revision();
        let due = revision != self.published_revision
            || (self.fed && (self.published_any || self.graphics.has_images()));
        if !due || !self.parser.is_ground() || self.shared.outstanding(self.seq) >= MAX_OUTSTANDING
        {
            return None;
        }
        let grid = self.term.grid();
        let history = grid.history_size();
        let (runs, images) =
            copad_graphics::collect(grid, history as i32, history + grid.screen_lines());
        self.seq += 1;
        self.published_revision = revision;
        self.published_any = !runs.is_empty();
        self.fed = false;
        self.shared.published.lock().unwrap().push_back(Published {
            seq: self.seq,
            history,
            runs,
            images,
        });
        Some(format!("\x1b]6;{MARKER_PREFIX}{}\x1b\\", self.seq).into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn split_all(chunks: &[&[u8]]) -> Vec<Segment> {
        let mut s = ApcSplitter::default();
        let mut out = VecDeque::new();
        for c in chunks {
            s.split(c, &mut out);
        }
        out.into_iter().collect()
    }

    fn feeds(segments: &[Segment]) -> Vec<u8> {
        segments
            .iter()
            .filter_map(|s| match s {
                Segment::Feed(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn apc_bodies_leave_the_stream_but_their_ends_stay() {
        let whole = split_all(&[b"a\x1b_Gi=1;AAAA\x1b\\b"]);
        assert_eq!(feeds(&whole), b"a\x1b_\x1b\\b");
        assert!(whole.contains(&Segment::Apc(b"Gi=1;AAAA".to_vec())));
        // The same at every split point.
        let input = b"x\x1b]0;t\x07\x1b_Gq=1\x1b\\y";
        for at in 0..input.len() {
            let parts = split_all(&[&input[..at], &input[at..]]);
            assert_eq!(feeds(&parts), b"x\x1b]0;t\x07\x1b_\x1b\\y", "split at {at}");
            assert!(
                parts.contains(&Segment::Apc(b"Gq=1".to_vec())),
                "split at {at}"
            );
        }
    }

    #[test]
    fn a_cancelled_apc_keeps_its_cancelling_bytes() {
        assert_eq!(feeds(&split_all(&[b"\x1b_Gx\x18z"])), b"\x1b_\x18z");
        assert_eq!(feeds(&split_all(&[b"\x1b_Gx\x1b[1mz"])), b"\x1b_\x1b[1mz");
        assert!(
            !split_all(&[b"\x1b_Gx\x18"])
                .iter()
                .any(|s| matches!(s, Segment::Apc(_)))
        );
        // An APC started by the cancelling ESC is its own command.
        assert!(split_all(&[b"\x1b_Gx\x1b_Gy\x1b\\"]).contains(&Segment::Apc(b"Gy".to_vec())));
    }

    fn shadow(cols: usize, rows: usize) -> Shadow {
        let shared = Arc::new(ImageShared::default());
        shared.graphics.enabled.store(true, Ordering::Relaxed);
        shared.graphics.cell_width.store(10, Ordering::Relaxed);
        shared.graphics.cell_height.store(20, Ordering::Relaxed);
        Shadow::new(cols, rows, shared)
    }

    fn transmit(sh: &mut Shadow, control: &str, w: usize, h: usize) -> Vec<u8> {
        let body = base64::engine::general_purpose::STANDARD.encode(vec![9u8; w * h * 3]);
        sh.run_apc(format!("G{control},f=24,s={w},v={h};{body}").as_bytes())
    }

    #[test]
    fn placements_replay_their_cursor_moves_and_scroll() {
        let mut sh = shadow(10, 4);
        sh.feed(b"\x1b[4;1H");
        // 10 x 3 cells from the last line: scroll 2, cursor to the last image row, edge -> LF.
        assert_eq!(
            transmit(&mut sh, "a=T,i=1,q=2", 100, 60),
            b"\x1b[2S\x1b[4;1H\n"
        );
        // C=1, queries and plain transmissions write nothing to VTE.
        assert!(transmit(&mut sh, "a=T,i=2,q=2,C=1", 10, 20).is_empty());
        assert!(transmit(&mut sh, "a=t,i=3,q=2", 10, 20).is_empty());
        // Origin mode: CUP counts from the region top.
        sh.feed(b"\x1b[2;4r\x1b[?6h\x1b[1;1H");
        assert_eq!(transmit(&mut sh, "a=T,i=4,q=2", 20, 20), b"\x1b[1;3H");
    }

    #[test]
    fn publications_wait_for_ground_and_for_acknowledgement() {
        let mut sh = shadow(10, 4);
        transmit(&mut sh, "a=T,i=1,q=2,C=1", 10, 20);
        // Mid-sequence: no marker may go in.
        sh.feed(b"\x1b[3");
        assert!(sh.publish().is_none());
        sh.feed(b"1m");
        let marker = sh.publish().expect("a placement publishes");
        assert_eq!(
            marker,
            format!("\x1b]6;{MARKER_PREFIX}1\x1b\\").into_bytes()
        );
        // More output while an image is up keeps publishing — up to the bound.
        for _ in 0..10 {
            sh.feed(b"\x1b[4;5Hx"); // away from the image, which text would erase
            sh.publish();
        }
        assert_eq!(sh.seq, MAX_OUTSTANDING);
        // VTE reached marker 2: publication 2 is shown, 1 dropped, room for more.
        let shown = sh.shared.acknowledge(2).unwrap();
        assert_eq!((shown.seq, shown.runs.len()), (2, 1));
        sh.feed(b"y");
        assert!(sh.publish().is_some());
        assert_eq!(marker_seq(&format!("{MARKER_PREFIX}17")), Some(17));
    }

    #[test]
    fn the_shadow_answers_graphics_queries() {
        let mut sh = shadow(10, 4);
        sh.run_apc(b"Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA");
        assert_eq!(sh.take_replies(), b"\x1b_Gi=31;OK\x1b\\");
    }
}
