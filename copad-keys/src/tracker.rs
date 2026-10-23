//! The kitty keyboard negotiation, read off a program's output.
//!
//! For a terminal widget that does not implement the protocol (VTE): every byte the program
//! writes passes through [`Tracker::feed`], which takes out the four negotiation sequences
//! (`CSI > flags u` push, `CSI < n u` pop, `CSI = flags ; mode u` set, `CSI ? u` query),
//! keeps the per-screen flag stacks, answers the query, and hands everything else on byte for
//! byte. It also follows the two modes the key encoder needs: DECCKM and which screen is up.
//!
//! The parser follows the DEC/ECMA-48 state machine VTE itself uses, so it agrees with the
//! widget on where a sequence starts and ends: ESC in any state starts a new escape (so ESC
//! ends a string, and `ESC \` is ST), CAN and SUB cancel, OSC ends at BEL or ST, while DCS, SOS,
//! PM and APC end only at ST — a BEL inside them is data. Nothing inside a string is ever
//! interpreted.

/// The most flag entries a screen's stack keeps; a push onto a full stack evicts the oldest.
const STACK_CAP: usize = 16;
/// A CSI longer than this is no negotiation sequence; it is passed through as it arrives.
const CSI_CAP: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Saw 0xC2 in ground: the next byte may make it a C1 control in UTF-8 form.
    GroundC2,
    Esc,
    /// `ESC` + intermediates, until the final byte.
    EscIntermediate,
    /// A CSI being held back until its final byte decides whether it is ours.
    Csi,
    /// A CSI known not to be ours (too long), passed through until its final byte.
    CsiPassthrough,
    Osc,
    /// DCS / SOS / PM / APC: ends only at ST.
    String,
    /// Saw 0xC2 inside a string: 0x9C makes it the C1 ST.
    StringC2 {
        osc: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Tracker {
    state: State,
    /// The held-back bytes of the sequence being read (ESC, or a whole CSI).
    held: Vec<u8>,
    /// The flag stacks of the screen that is up and of the other one.
    active: Vec<u8>,
    inactive: Vec<u8>,
    alt_screen: bool,
    app_cursor: bool,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            held: Vec::with_capacity(CSI_CAP),
            active: Vec::new(),
            inactive: Vec::new(),
            alt_screen: false,
            app_cursor: false,
        }
    }

    /// The flags in force on the screen that is up.
    pub fn flags(&self) -> u8 {
        self.active.last().copied().unwrap_or(0)
    }

    /// DECCKM: unmodified cursor keys as `SS3`.
    pub fn app_cursor(&self) -> bool {
        self.app_cursor
    }

    /// Feed program output. What the terminal should see is appended to `out`; answers to send
    /// back to the program are appended to `replies`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, replies: &mut Vec<u8>) {
        for &b in input {
            self.step(b, out, replies);
        }
    }

    fn flush_held(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>, replies: &mut Vec<u8>) {
        // Anywhere: CAN / SUB abandon a sequence, ESC starts a new one.
        match b {
            0x18 | 0x1a => {
                self.flush_held(out);
                out.push(b);
                self.state = State::Ground;
                return;
            }
            0x1b => {
                self.flush_held(out);
                self.held.push(b);
                self.state = State::Esc;
                return;
            }
            _ => {}
        }
        match self.state {
            State::Ground => {
                if b == 0xc2 {
                    self.held.push(b);
                    self.state = State::GroundC2;
                } else {
                    out.push(b);
                }
            }
            State::GroundC2 => {
                if b == 0x9b {
                    self.held.push(b);
                    self.state = State::Csi;
                    return;
                }
                self.flush_held(out);
                if b == 0xc2 {
                    // Another lead byte: it may itself start a C1 control.
                    self.held.push(b);
                    return;
                }
                out.push(b);
                self.state = match b {
                    0x9d => State::Osc,
                    0x90 | 0x98 | 0x9e | 0x9f => State::String,
                    _ => State::Ground,
                };
            }
            State::Esc => match b {
                b'[' => {
                    self.held.push(b);
                    self.state = State::Csi;
                }
                b']' => {
                    self.flush_held(out);
                    out.push(b);
                    self.state = State::Osc;
                }
                b'P' | b'X' | b'^' | b'_' => {
                    self.flush_held(out);
                    out.push(b);
                    self.state = State::String;
                }
                0x20..=0x2f => {
                    self.flush_held(out);
                    out.push(b);
                    self.state = State::EscIntermediate;
                }
                // A C0 control inside an escape is executed and the escape goes on.
                0x00..=0x1f => out.push(b),
                _ => {
                    self.flush_held(out);
                    out.push(b);
                    if b == b'c' {
                        self.reset();
                    }
                    self.state = State::Ground;
                }
            },
            State::EscIntermediate => {
                out.push(b);
                if (0x30..=0x7e).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::Csi => {
                if b < 0x20 {
                    // C0 controls are executed in the middle of a CSI; their order relative
                    // to the held prefix does not matter to the parser downstream.
                    out.push(b);
                } else if (0x40..=0x7e).contains(&b) {
                    self.held.push(b);
                    self.finish_csi(out, replies);
                    self.state = State::Ground;
                } else if b >= 0x80 {
                    self.flush_held(out);
                    out.push(b);
                    self.state = State::Ground;
                } else if self.held.len() >= CSI_CAP {
                    self.flush_held(out);
                    out.push(b);
                    self.state = State::CsiPassthrough;
                } else {
                    self.held.push(b);
                }
            }
            State::CsiPassthrough => {
                out.push(b);
                if (0x40..=0x7e).contains(&b) || b >= 0x80 {
                    self.state = State::Ground;
                }
            }
            State::Osc | State::String => {
                out.push(b);
                let osc = self.state == State::Osc;
                if osc && b == 0x07 {
                    self.state = State::Ground;
                } else if b == 0xc2 {
                    self.state = State::StringC2 { osc };
                }
            }
            State::StringC2 { osc } => {
                out.push(b);
                self.state = match b {
                    0x9c => State::Ground,
                    0xc2 => State::StringC2 { osc },
                    0x07 if osc => State::Ground,
                    _ if osc => State::Osc,
                    _ => State::String,
                };
            }
        }
    }

    /// A complete CSI is held in `self.held`: consume it if it is a negotiation sequence,
    /// otherwise pass it on (noting the modes it sets).
    fn finish_csi(&mut self, out: &mut Vec<u8>, replies: &mut Vec<u8>) {
        let held = std::mem::take(&mut self.held);
        // After `ESC [` or its C1 form `C2 9B`, both two bytes.
        let body = &held[2..];
        let (&fin, inner) = body.split_last().expect("a CSI ends in its final byte");
        let (prefix, params) = match inner.first() {
            Some(&p @ 0x3c..=0x3f) => (Some(p), &inner[1..]),
            _ => (None, inner),
        };
        let digits_only = params.iter().all(|&c| c.is_ascii_digit() || c == b';');
        if fin == b'u'
            && digits_only
            && let Some(p) = prefix
        {
            let nums = parse_params(params);
            let n = |i: usize, default: u32| nums.get(i).copied().flatten().unwrap_or(default);
            match p {
                b'?' => {
                    replies.extend_from_slice(format!("\x1b[?{}u", self.flags()).as_bytes());
                }
                b'>' => self.push(n(0, 0) as u8),
                b'<' => self.pop(n(0, 1) as usize),
                b'=' => self.set(n(0, 0) as u8, n(1, 1)),
                _ => {}
            }
            return;
        }
        if digits_only && (fin == b'h' || fin == b'l') && prefix == Some(b'?') {
            let on = fin == b'h';
            for mode in parse_params(params).into_iter().flatten() {
                match mode {
                    1 => self.app_cursor = on,
                    47 | 1047 | 1049 => self.set_alt_screen(on),
                    _ => {}
                }
            }
        }
        // DECSTR (`CSI ! p`) is a soft reset: cursor keys go back to normal.
        if fin == b'p' && prefix.is_none() && params == b"!" {
            self.app_cursor = false;
        }
        out.extend_from_slice(&held);
    }

    fn push(&mut self, flags: u8) {
        if self.active.len() >= STACK_CAP {
            self.active.remove(0);
        }
        self.active.push(flags & crate::flags::ALL);
    }

    fn pop(&mut self, n: usize) {
        let keep = self.active.len().saturating_sub(n);
        self.active.truncate(keep);
    }

    fn set(&mut self, flags: u8, mode: u32) {
        let flags = flags & crate::flags::ALL;
        let cur = self.flags();
        let new = match mode {
            1 => flags,
            2 => cur | flags,
            3 => cur & !flags,
            _ => return,
        };
        match self.active.last_mut() {
            Some(top) => *top = new,
            None => self.active.push(new),
        }
    }

    fn set_alt_screen(&mut self, on: bool) {
        if on != self.alt_screen {
            std::mem::swap(&mut self.active, &mut self.inactive);
            self.alt_screen = on;
        }
    }

    /// RIS (`ESC c`): back to the power-on state.
    fn reset(&mut self) {
        self.active.clear();
        self.inactive.clear();
        self.alt_screen = false;
        self.app_cursor = false;
    }
}

/// `1;;3` → `[Some(1), None, Some(3)]`.
fn parse_params(params: &[u8]) -> Vec<Option<u32>> {
    if params.is_empty() {
        return Vec::new();
    }
    params
        .split(|&c| c == b';')
        .map(|p| {
            if p.is_empty() {
                None
            } else {
                std::str::from_utf8(p).ok()?.parse::<u32>().ok()
            }
        })
        .collect()
}
