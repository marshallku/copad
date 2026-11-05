//! What a shell command an agent ran did to files (decisions #205): the reads, searches, edits,
//! writes and deletes the work graph can attribute to a Claude `Bash` call or a Codex command,
//! without running anything and without looking at today's filesystem.
//!
//! **Conservative by construction.** Only direct commands with literal operands are classified:
//! a word holding `$`, a glob or brace expansion is never taken for a path, and every
//! recognised command has an explicit option table — an option outside it (`--help` included)
//! makes that stage opaque. A construct the parser does not model (`||`, `&`, a subshell or
//! group, `$(…)`, a backtick, process substitution, a reserved word in command position) STOPS
//! the parse: nothing after it is read.
//!
//! **Certainty.** A command's exit status says little about its parts, so each effect carries
//! what a successful exit proves about it ([`Effect::on_ok`]). With chains split by `;`/newline,
//! pipelines joined by `&&` and stages by `|`:
//! - a stage certainly RAN if its pipeline is the first of its chain, or — when the parse did
//!   not stop — it is in the FINAL chain (an `&&` chain that exited 0 ran all of it);
//! - it certainly SUCCEEDED only as the last stage of a pipeline in the final chain of a parse
//!   that did not stop.
//!
//! SUCCEEDED gives `Ok`, RAN only gives `Unknown` (shown, never counted), anything else is
//! dropped — redirections included, since an earlier one failing skips the rest.
//!
//! **cwd.** A standalone `cd LITERAL` moves the directory for the rest of its `&&` chain (one
//! to where the shell already is moves nothing). At the end of a chain that held one, or the
//! moment any stage could move it in a way not modelled (`cd` inside a pipeline, `pushd`,
//! `eval`, `source`, `exec`, `command`, …), the directory is UNKNOWN and relative operands are
//! dropped from then on.

use crate::workgraph::normalize;
use copad_mux_proto::graph::{Outcome, StepKind};

#[derive(Debug, Clone, PartialEq)]
pub struct Effect {
    pub kind: StepKind,
    pub path: String,
    pub label: Option<String>,
    pub on_ok: Outcome,
}

#[derive(Debug, Default, PartialEq)]
pub struct Analysis {
    pub effects: Vec<Effect>,
    /// Some part was not classified (an unknown command, an option outside the table, a stop):
    /// the call still deserves its own `Run` step.
    pub opaque: bool,
}

/// Analyse `script` run in `cwd`. `background`: the call's result arrives before the command
/// ends (Claude's `run_in_background`), so its exit status proves nothing.
pub fn analyze(script: &str, cwd: &str, home: &str, background: bool) -> Analysis {
    let (chains, stopped) = parse(script);
    let stopped = stopped || background;
    let mut out = Analysis {
        opaque: stopped,
        ..Default::default()
    };
    let mut dir = (!cwd.is_empty()).then(|| cwd.to_string());
    let last = chains.len().saturating_sub(1);
    for (ci, chain) in chains.iter().enumerate() {
        let is_final = !stopped && ci == last;
        let mut chain_cd = false;
        for (pi, pipeline) in chain.iter().enumerate() {
            let ran = pi == 0 || is_final;
            // A standalone `cd` moves the shell; inside a pipeline it may not (bash) or may
            // (zsh's last stage), so there it only makes the directory unknown.
            if let [stage] = pipeline.as_slice()
                && let Some(to) = stage.cd_target()
            {
                // Its redirections are opened before it runs, where the shell still is.
                out.effects
                    .extend(redirect_effects(stage, dir.as_deref(), home, ran, is_final));
                let target = to.and_then(|t| resolve(dir.as_deref(), &t, home));
                // `cd` to where the shell already is changes nothing, whether it succeeds or
                // not (agents open most commands with `cd <project>;`).
                if target.is_none() || target != dir {
                    chain_cd = true;
                    dir = if ran { target } else { None };
                }
                continue;
            }
            let n = pipeline.len();
            for (si, stage) in pipeline.iter().enumerate() {
                if stage.moves_cwd() {
                    chain_cd = true;
                    dir = None;
                }
                let succeeded = is_final && si + 1 == n;
                let on_ok = if succeeded {
                    Outcome::Ok
                } else if ran {
                    Outcome::Unknown
                } else {
                    continue;
                };
                let first = si == 0
                    && !stage.stdin
                    && !stage.redirs.iter().any(|r| r.kind == StepKind::Read);
                match classify(stage, first) {
                    Some(effects) => {
                        for (kind, path, label) in effects {
                            let Some(abs) = resolve(dir.as_deref(), &path, home) else {
                                continue;
                            };
                            out.effects.push(Effect {
                                kind,
                                path: abs,
                                label,
                                on_ok,
                            });
                        }
                    }
                    None => out.opaque = true,
                }
                out.effects.extend(redirect_effects(
                    stage,
                    dir.as_deref(),
                    home,
                    ran,
                    succeeded,
                ));
            }
        }
        if chain_cd {
            dir = None;
        }
    }
    out
}

fn redirect_effects(
    stage: &Stage,
    dir: Option<&str>,
    home: &str,
    ran: bool,
    succeeded: bool,
) -> Vec<Effect> {
    let on_ok = match (succeeded, ran) {
        (true, _) => Outcome::Ok,
        (false, true) => Outcome::Unknown,
        _ => return Vec::new(),
    };
    stage
        .redirs
        .iter()
        .filter_map(|r| {
            let path = r.target.path()?;
            if path.starts_with("/dev/") {
                return None;
            }
            Some(Effect {
                kind: r.kind,
                path: resolve(dir, &path, home)?,
                label: None,
                on_ok,
            })
        })
        .collect()
}

fn resolve(dir: Option<&str>, path: &str, home: &str) -> Option<String> {
    let path = expand_tilde(path, home)?;
    if path.starts_with('/') {
        return Some(normalize("", &path));
    }
    Some(normalize(dir?, &path))
}

fn expand_tilde(path: &str, home: &str) -> Option<String> {
    if path == "~" {
        return (!home.is_empty()).then(|| home.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return (!home.is_empty()).then(|| format!("{home}/{rest}"));
    }
    Some(path.to_string())
}

/// One shell word: its text with quoting removed, and whether that text is exactly what the
/// shell will pass (no expansion of any kind left in it).
#[derive(Debug, Clone, Default, PartialEq)]
struct Word {
    text: String,
    literal: bool,
    quoted: bool,
    /// Starts with an unquoted `~` that expands (`~` or `~/…`); `text` keeps the `~`.
    tilde: bool,
    assign: bool,
}

impl Word {
    fn literal(&self) -> Option<&str> {
        self.literal.then_some(self.text.as_str())
    }

    /// The word as a path for [`resolve`]: a `~` the shell does not expand (quoted) is kept
    /// from being expanded there by making it explicitly relative.
    fn path(&self) -> Option<String> {
        let t = self.literal()?;
        if t.starts_with('~') && !self.tilde {
            return Some(format!("./{t}"));
        }
        Some(t.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Redir {
    /// `Read` (`<`), `Write` (`>`, `>|`, `&>`) or `Edit` (`>>`, `&>>`).
    kind: StepKind,
    target: Word,
}

#[derive(Debug, Default, PartialEq)]
struct Stage {
    words: Vec<Word>,
    redirs: Vec<Redir>,
    stdin: bool,
}

impl Stage {
    fn argv(&self) -> &[Word] {
        let skip = self.words.iter().take_while(|w| w.assign).count();
        &self.words[skip..]
    }

    fn name(&self) -> Option<&str> {
        let w = self.argv().first()?;
        let t = w.literal()?;
        Some(t.rsplit('/').next().unwrap_or(t))
    }

    /// `Some(target)` for a `cd` this parser can follow: `Some(Some(dir))` for `cd DIR`,
    /// `Some(None)` for any other `cd` (the directory becomes unknown).
    fn cd_target(&self) -> Option<Option<String>> {
        if self.name()? != "cd" {
            return None;
        }
        Some(match &self.argv()[1..] {
            [] => Some("~".to_string()),
            [w] if w.literal && !w.text.starts_with('-') => w.path(),
            _ => None,
        })
    }

    fn moves_cwd(&self) -> bool {
        match self.argv().first() {
            None => false,
            Some(w) if !w.literal => true,
            Some(_) => matches!(
                self.name(),
                Some(
                    "cd" | "pushd"
                        | "popd"
                        | "eval"
                        | "source"
                        | "."
                        | "exec"
                        | "command"
                        | "builtin"
                        | "z"
                        | "zi"
                        | "j"
                )
            ),
        }
    }
}

type Pipeline = Vec<Stage>;
type Chain = Vec<Pipeline>;

#[derive(Debug, PartialEq)]
enum Tok {
    Word(Word),
    Redir(Redir),
    Inert,
    Stdin,
    And,
    Pipe,
    Seq,
    /// Parsing stops here; the stage before it is complete (`||`, a background `&`, `;;`).
    StopAfter,
    Stop,
}

const RESERVED: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "coproc", "{", "}", "[[", "]]", "!",
];

fn parse(script: &str) -> (Vec<Chain>, bool) {
    let toks = lex(script);
    let mut chains: Vec<Chain> = Vec::new();
    let mut chain: Chain = Vec::new();
    let mut pipeline: Pipeline = Vec::new();
    let mut stage = Stage::default();
    let mut stopped = false;
    let empty = |s: &Stage| s.words.is_empty() && s.redirs.is_empty();
    for t in toks {
        match t {
            Tok::Word(w) => {
                if stage.argv().is_empty()
                    && !w.assign
                    && !w.quoted
                    && RESERVED.contains(&w.text.as_str())
                {
                    stopped = true;
                    break;
                }
                stage.words.push(w);
            }
            Tok::Redir(r) => stage.redirs.push(r),
            Tok::Inert => {}
            Tok::Stdin => stage.stdin = true,
            Tok::Pipe | Tok::And => {
                if empty(&stage) {
                    stopped = true;
                    break;
                }
                pipeline.push(std::mem::take(&mut stage));
                if t == Tok::And {
                    chain.push(std::mem::take(&mut pipeline));
                }
            }
            Tok::Seq => {
                if empty(&stage) {
                    if !pipeline.is_empty() {
                        stopped = true;
                        break;
                    }
                } else {
                    pipeline.push(std::mem::take(&mut stage));
                }
                if !pipeline.is_empty() {
                    chain.push(std::mem::take(&mut pipeline));
                }
                if !chain.is_empty() {
                    chains.push(std::mem::take(&mut chain));
                }
            }
            Tok::StopAfter => {
                if !empty(&stage) {
                    pipeline.push(std::mem::take(&mut stage));
                }
                stopped = true;
                break;
            }
            Tok::Stop => {
                stopped = true;
                break;
            }
        }
    }
    if stopped {
        // Keep what was complete before the stop: whole stages of the open chain, never a
        // half-read one.
        if !pipeline.is_empty() {
            chain.push(pipeline);
        }
    } else {
        if !empty(&stage) {
            pipeline.push(stage);
        } else if !pipeline.is_empty() {
            // `a |` or `a &&` with nothing after it.
            stopped = true;
        }
        if !pipeline.is_empty() {
            chain.push(pipeline);
        }
    }
    if !chain.is_empty() {
        chains.push(chain);
    }
    (chains, stopped)
}

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
    heredocs: Vec<(String, bool)>,
    out: Vec<Tok>,
}

fn lex(script: &str) -> Vec<Tok> {
    let mut lx = Lexer {
        s: script.as_bytes(),
        i: 0,
        heredocs: Vec::new(),
        out: Vec::new(),
    };
    lx.run();
    lx.out
}

impl Lexer<'_> {
    fn peek(&self, k: usize) -> Option<u8> {
        self.s.get(self.i + k).copied()
    }

    fn starts(&self, p: &str) -> bool {
        self.s[self.i..].starts_with(p.as_bytes())
    }

    fn run(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'\\' if self.peek(1) == Some(b'\n') => self.i += 2,
                b'\n' => {
                    self.i += 1;
                    self.out.push(Tok::Seq);
                    if !self.heredoc_bodies() {
                        self.out.push(Tok::Stop);
                        return;
                    }
                }
                b'#' => {
                    while self.peek(0).is_some_and(|c| c != b'\n') {
                        self.i += 1;
                    }
                }
                b'|' => {
                    if self.starts("||") {
                        self.out.push(Tok::StopAfter);
                        return;
                    }
                    self.i += if self.starts("|&") { 2 } else { 1 };
                    self.out.push(Tok::Pipe);
                }
                b'&' => {
                    if self.starts("&&") {
                        self.i += 2;
                        self.out.push(Tok::And);
                    } else if self.starts("&>") {
                        let append = self.starts("&>>");
                        self.i += if append { 3 } else { 2 };
                        if !self.redir_target(if append {
                            StepKind::Edit
                        } else {
                            StepKind::Write
                        }) {
                            return;
                        }
                    } else {
                        self.out.push(Tok::StopAfter);
                        return;
                    }
                }
                b';' => {
                    if self.starts(";;") || self.starts(";&") {
                        self.out.push(Tok::StopAfter);
                        return;
                    }
                    self.i += 1;
                    self.out.push(Tok::Seq);
                }
                b'(' | b')' => {
                    self.out.push(Tok::Stop);
                    return;
                }
                b'<' | b'>' => {
                    if !self.redirection() {
                        return;
                    }
                }
                _ => {
                    let Some(w) = self.word() else {
                        self.out.push(Tok::Stop);
                        return;
                    };
                    // `2>file`: an all-digit word glued to a redirection is its fd.
                    if !w.quoted
                        && !w.text.is_empty()
                        && w.text.bytes().all(|b| b.is_ascii_digit())
                        && matches!(self.peek(0), Some(b'<' | b'>'))
                    {
                        continue;
                    }
                    self.out.push(Tok::Word(w));
                }
            }
        }
        if !self.heredocs.is_empty() {
            self.out.push(Tok::Stop);
        }
    }

    /// A redirection at `<`/`>`. False when it stopped the parse.
    fn redirection(&mut self) -> bool {
        if self.starts("<(") || self.starts(">(") || self.starts("<>") {
            self.out.push(Tok::Stop);
            return false;
        }
        if self.starts("<<<") {
            self.i += 3;
            self.skip_blanks();
            if self.word().is_none() {
                self.out.push(Tok::Stop);
                return false;
            }
            self.out.push(Tok::Stdin);
            return true;
        }
        if self.starts("<<") {
            self.i += 2;
            let strip = self.peek(0) == Some(b'-');
            if strip {
                self.i += 1;
            }
            self.skip_blanks();
            let Some(delim) = self.word() else {
                self.out.push(Tok::Stop);
                return false;
            };
            if delim.text.is_empty() {
                self.out.push(Tok::Stop);
                return false;
            }
            self.heredocs.push((delim.text, strip));
            self.out.push(Tok::Stdin);
            return true;
        }
        if self.starts("<&") || self.starts(">&") {
            self.i += 2;
            self.skip_blanks();
            // `>&2`, `<&-`: a descriptor. `>& file` is bash for `&>`.
            if self
                .peek(0)
                .is_some_and(|c| c.is_ascii_digit() || c == b'-')
            {
                return self.skip_word();
            }
            return self.redir_target(StepKind::Write);
        }
        let kind = if self.starts(">>") {
            self.i += 2;
            StepKind::Edit
        } else if self.starts(">|") {
            self.i += 2;
            StepKind::Write
        } else if self.starts(">") {
            self.i += 1;
            StepKind::Write
        } else {
            self.i += 1;
            StepKind::Read
        };
        self.redir_target(kind)
    }

    fn redir_target(&mut self, kind: StepKind) -> bool {
        self.skip_blanks();
        match self.word() {
            Some(w) if !w.text.is_empty() || w.quoted => {
                self.out.push(Tok::Redir(Redir { kind, target: w }));
                true
            }
            _ => {
                self.out.push(Tok::Stop);
                false
            }
        }
    }

    fn skip_word(&mut self) -> bool {
        self.skip_blanks();
        if self.word().is_none() {
            self.out.push(Tok::Stop);
            return false;
        }
        self.out.push(Tok::Inert);
        true
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(0), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    fn heredoc_bodies(&mut self) -> bool {
        for (delim, strip) in std::mem::take(&mut self.heredocs) {
            loop {
                if self.i >= self.s.len() {
                    return false;
                }
                let end = self.s[self.i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(self.s.len(), |p| self.i + p);
                let line = String::from_utf8_lossy(&self.s[self.i..end]);
                let line = if strip {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                let done = line.trim_end_matches('\r') == delim;
                self.i = (end + 1).min(self.s.len());
                if done {
                    break;
                }
            }
        }
        true
    }

    /// One word at the cursor. `None` for a construct that stops the parse (an unterminated
    /// quote, `$(…)`, a backtick).
    fn word(&mut self) -> Option<Word> {
        let mut w = Word {
            literal: true,
            ..Default::default()
        };
        let start = self.i;
        let mut name_prefix = true;
        if self.peek(0) == Some(b'~') {
            match self.peek(1) {
                None | Some(b'/' | b' ' | b'\t' | b'\n' | b';' | b'|' | b'&' | b')') => {
                    w.tilde = true
                }
                _ => w.literal = false,
            }
        }
        if self.peek(0) == Some(b'=') {
            // zsh `=cmd` expansion.
            w.literal = false;
        }
        while let Some(c) = self.peek(0) {
            match c {
                b' ' | b'\t' | b'\r' | b'\n' | b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' => {
                    break;
                }
                b'\'' => {
                    w.quoted = true;
                    name_prefix = false;
                    self.i += 1;
                    let end = self.s[self.i..].iter().position(|&b| b == b'\'')?;
                    w.text
                        .push_str(&String::from_utf8_lossy(&self.s[self.i..self.i + end]));
                    self.i += end + 1;
                }
                b'"' => {
                    w.quoted = true;
                    name_prefix = false;
                    self.i += 1;
                    loop {
                        match self.peek(0)? {
                            b'"' => {
                                self.i += 1;
                                break;
                            }
                            b'\\' => {
                                let n = self.peek(1)?;
                                if matches!(n, b'$' | b'`' | b'"' | b'\\' | b'\n') {
                                    if n != b'\n' {
                                        w.text.push(n as char);
                                    }
                                } else {
                                    w.text.push('\\');
                                    self.push_char(&mut w, 1);
                                    continue;
                                }
                                self.i += 2;
                            }
                            b'`' => return None,
                            b'$' => {
                                if self.peek(1) == Some(b'(') {
                                    return None;
                                }
                                w.literal = false;
                                w.text.push('$');
                                self.i += 1;
                            }
                            _ => self.push_char(&mut w, 0),
                        }
                    }
                }
                b'\\' => {
                    name_prefix = false;
                    match self.peek(1) {
                        Some(b'\n') => self.i += 2,
                        Some(_) => {
                            self.i += 1;
                            w.quoted = true;
                            self.push_char(&mut w, 0);
                        }
                        None => {
                            self.i += 1;
                        }
                    }
                }
                b'`' => return None,
                b'$' => {
                    match self.peek(1) {
                        Some(b'(') => return None,
                        Some(b'\'') => {
                            // `$'…'`: ANSI-C quoting, taken as not literal.
                            self.i += 2;
                            w.literal = false;
                            loop {
                                match self.peek(0)? {
                                    b'\\' => self.i += 2,
                                    b'\'' => {
                                        self.i += 1;
                                        break;
                                    }
                                    _ => self.i += 1,
                                }
                            }
                            continue;
                        }
                        Some(b'{') => {
                            let end = self.s[self.i..].iter().position(|&b| b == b'}')?;
                            self.i += end + 1;
                        }
                        _ => self.i += 1,
                    }
                    w.literal = false;
                    name_prefix = false;
                    w.text.push('$');
                }
                b'=' if name_prefix && self.i > start => {
                    w.assign = true;
                    name_prefix = false;
                    w.text.push('=');
                    self.i += 1;
                }
                b'*' | b'?' | b'[' | b'{' | b'}' => {
                    w.literal = false;
                    name_prefix = false;
                    w.text.push(c as char);
                    self.i += 1;
                }
                _ => {
                    if name_prefix
                        && !(c == b'_'
                            || c.is_ascii_alphabetic()
                            || (c.is_ascii_digit() && self.i > start))
                    {
                        name_prefix = false;
                    }
                    self.push_char(&mut w, 0);
                }
            }
        }
        if w.tilde && w.quoted && !w.text.starts_with('~') {
            w.tilde = false;
        }
        Some(w)
    }

    fn push_char(&mut self, w: &mut Word, skip: usize) {
        let at = self.i + skip;
        let len = utf8_len(self.s[at]);
        let end = (at + len).min(self.s.len());
        w.text.push_str(&String::from_utf8_lossy(&self.s[at..end]));
        self.i = end;
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

struct Spec {
    flags: &'static str,
    valued: &'static str,
    long: &'static [&'static str],
    long_valued: &'static [&'static str],
    numeric: bool,
}

type Parsed<'a> = (Vec<(String, Option<&'a Word>)>, Vec<&'a Word>);

/// Split `args` into options and operands by `spec` (GNU-style: options may follow operands
/// until `--`). `None` when an option is outside the table or not literal.
fn options<'a>(args: &'a [Word], spec: &Spec) -> Option<Parsed<'a>> {
    let mut opts = Vec::new();
    let mut operands = Vec::new();
    let mut it = args.iter();
    let mut ended = false;
    while let Some(w) = it.next() {
        let t = w.text.as_str();
        if ended || !(t.starts_with('-') && t.len() > 1) {
            operands.push(w);
            continue;
        }
        // Only a long option's `=value` may hold an expansion (`--include=*.rs`).
        let name_clean = t
            .strip_prefix("--")
            .and_then(|l| l.split_once('='))
            .is_some_and(|(n, _)| n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
        if !w.literal && !name_clean {
            return None;
        }
        if t == "--" {
            ended = true;
            continue;
        }
        if let Some(long) = t.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, _)) => (n, Some(w)),
                None => (long, None),
            };
            if spec.long_valued.contains(&name) {
                let v = match value {
                    Some(v) => v,
                    None => it.next()?,
                };
                opts.push((name.to_string(), Some(v)));
            } else if spec.long.contains(&name) {
                opts.push((name.to_string(), value));
            } else {
                return None;
            }
            continue;
        }
        let body = &t[1..];
        if spec.numeric && body.bytes().all(|b| b.is_ascii_digit()) {
            opts.push(("NUM".to_string(), Some(w)));
            continue;
        }
        for (k, c) in body.char_indices() {
            if spec.valued.contains(c) {
                let v = if k + c.len_utf8() < body.len() {
                    w
                } else {
                    it.next()?
                };
                opts.push((c.to_string(), Some(v)));
                break;
            }
            if !spec.flags.contains(c) {
                return None;
            }
            opts.push((c.to_string(), None));
        }
    }
    Some((opts, operands))
}

fn paths(words: &[&Word]) -> Vec<String> {
    words
        .iter()
        .filter(|w| w.text != "-" && !w.text.is_empty())
        .filter_map(|w| w.path())
        .collect()
}

type Found = Vec<(StepKind, String, Option<String>)>;

fn each(kind: StepKind, ps: Vec<String>) -> Found {
    ps.into_iter().map(|p| (kind, p, None)).collect()
}

/// The file effects of one stage's command, or `None` when it is not classified.
/// `first`: the stage reads no pipe or redirection, so a search without a path searches the
/// working directory.
fn classify(stage: &Stage, first: bool) -> Option<Found> {
    let argv = stage.argv();
    let Some(cmd) = stage.name() else {
        return (argv.is_empty()).then(Vec::new);
    };
    let args = &argv[1..];
    let read = |spec: &Spec| Some(each(StepKind::Read, paths(&options(args, spec)?.1)));
    match cmd {
        "cat" => read(&Spec {
            flags: "benstuvAET",
            valued: "",
            long: &[
                "number",
                "number-nonblank",
                "show-all",
                "squeeze-blank",
                "show-ends",
                "show-tabs",
            ],
            long_valued: &[],
            numeric: false,
        }),
        "head" => read(&Spec {
            flags: "qv",
            valued: "nc",
            long: &["quiet", "silent", "verbose"],
            long_valued: &["lines", "bytes"],
            numeric: true,
        }),
        "tail" => read(&Spec {
            flags: "fFqrv",
            valued: "nc",
            long: &["quiet", "silent", "verbose", "follow", "retry"],
            long_valued: &["lines", "bytes"],
            numeric: true,
        }),
        "nl" => read(&Spec {
            flags: "p",
            valued: "bdfhilnsvw",
            long: &[],
            long_valued: &["body-numbering", "number-width", "number-format"],
            numeric: false,
        }),
        "wc" => read(&Spec {
            flags: "clmwL",
            valued: "",
            long: &["lines", "bytes", "chars", "words"],
            long_valued: &[],
            numeric: false,
        }),
        "less" | "more" => read(&Spec {
            flags: "NRSXFiImMnqQrsu",
            valued: "",
            long: &[],
            long_valued: &[],
            numeric: false,
        }),
        "bat" | "batcat" => read(&Spec {
            flags: "pnAPu",
            valued: "lHr",
            long: &[
                "plain",
                "number",
                "show-all",
                "paging",
                "style",
                "color",
                "decorations",
            ],
            long_valued: &[
                "language",
                "line-range",
                "highlight-line",
                "theme",
                "tabs",
                "wrap",
            ],
            numeric: false,
        }),
        "diff" => read(&Spec {
            flags: "uqNwbBiaEtTsy",
            valued: "UC",
            long: &[
                "unified",
                "brief",
                "color",
                "ignore-all-space",
                "ignore-space-change",
                "ignore-blank-lines",
                "ignore-case",
                "text",
                "side-by-side",
                "strip-trailing-cr",
                "report-identical-files",
                "new-file",
            ],
            long_valued: &["label"],
            numeric: false,
        }),
        "cmp" => read(&Spec {
            flags: "lsb",
            valued: "in",
            long: &["silent", "quiet", "verbose", "print-bytes"],
            long_valued: &["ignore-initial", "bytes"],
            numeric: false,
        }),
        "sed" | "gsed" => sed(args),
        "grep" | "egrep" | "fgrep" | "ggrep" => grep(args, first),
        "rg" => rg(args, first),
        "find" | "gfind" => find(args),
        "ls" | "eza" => {
            let (_, ops) = options(
                args,
                &Spec {
                    flags: "1aAlhtrSdFGCpQikRgonsuUcBX",
                    valued: "",
                    long: &[
                        "all",
                        "almost-all",
                        "human-readable",
                        "color",
                        "classify",
                        "reverse",
                        "directory",
                        "group-directories-first",
                        "recursive",
                        "sort",
                        "tree",
                        "git",
                        "long",
                        "icons",
                    ],
                    long_valued: &["level"],
                    numeric: false,
                },
            )?;
            let ps = paths(&ops);
            if ps.is_empty() {
                return Some(vec![(StepKind::Search, ".".to_string(), None)]);
            }
            Some(each(StepKind::Search, ps))
        }
        "tee" => {
            let (opts, ops) = options(
                args,
                &Spec {
                    flags: "aip",
                    valued: "",
                    long: &["append", "ignore-interrupts"],
                    long_valued: &[],
                    numeric: false,
                },
            )?;
            let append = opts.iter().any(|(n, _)| n == "a" || n == "append");
            let kind = if append {
                StepKind::Edit
            } else {
                StepKind::Write
            };
            Some(each(kind, paths(&ops)))
        }
        "rm" => {
            let (_, ops) = options(
                args,
                &Spec {
                    // Not `-i`/`-I`: a declined prompt still exits 0.
                    flags: "fv",
                    valued: "",
                    long: &["force", "verbose"],
                    long_valued: &[],
                    numeric: false,
                },
            )?;
            Some(each(StepKind::Delete, paths(&ops)))
        }
        // File-less: classified, so they do not force a Run step on their own.
        "echo" | "printf" | "true" | ":" | "pwd" | "date" | "sleep" | "basename" | "dirname"
        | "tr" => Some(Vec::new()),
        // Filters of a pipe; given an operand they read a file, which is not modelled.
        "sort" | "uniq" | "cut" | "column" | "xxd" | "od"
            if args.iter().all(|w| w.text.starts_with('-')) =>
        {
            Some(Vec::new())
        }
        "jq" if args.iter().filter(|w| !w.text.starts_with('-')).count() <= 1 => Some(Vec::new()),
        _ => None,
    }
}

fn sed(args: &[Word]) -> Option<Found> {
    let mut inplace = false;
    let mut scripts: Vec<&Word> = Vec::new();
    let mut operands: Vec<&Word> = Vec::new();
    let mut i = 0;
    let mut ended = false;
    while i < args.len() {
        let w = &args[i];
        i += 1;
        let t = w.text.as_str();
        if ended || !(t.starts_with('-') && t.len() > 1) {
            operands.push(w);
            continue;
        }
        if !w.literal {
            return None;
        }
        if t == "--" {
            ended = true;
            continue;
        }
        if let Some(long) = t.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            match name {
                "in-place" => inplace = true,
                "quiet" | "silent" | "regexp-extended" | "posix" | "separate" | "unbuffered"
                | "null-data" | "debug" | "sandbox" | "line-length" => {}
                "expression" => match value {
                    Some(_) => return None,
                    None => {
                        scripts.push(args.get(i)?);
                        i += 1;
                    }
                },
                _ => return None,
            }
            continue;
        }
        let body = &t[1..];
        for (k, c) in body.char_indices() {
            let rest = &body[k + 1..];
            match c {
                'n' | 'E' | 'r' | 's' | 'u' | 'z' => {}
                'i' | 'I' => {
                    inplace = true;
                    if rest.is_empty()
                        && let Some(next) = args.get(i)
                        && (next.text.is_empty() && next.quoted || next.text.starts_with('.'))
                    {
                        // BSD `-i ''` / `-i .bak`: the next word is the suffix.
                        i += 1;
                    }
                    break;
                }
                'e' => {
                    if !rest.is_empty() {
                        return None;
                    }
                    scripts.push(args.get(i)?);
                    i += 1;
                    break;
                }
                'l' => {
                    if rest.is_empty() {
                        i += 1;
                    }
                    break;
                }
                _ => return None,
            }
        }
    }
    if scripts.is_empty() {
        if operands.is_empty() {
            return None;
        }
        scripts.push(operands.remove(0));
    }
    if !scripts
        .iter()
        .all(|s| s.literal().is_some_and(sed_script_safe))
    {
        return None;
    }
    let kind = if inplace {
        StepKind::Edit
    } else {
        StepKind::Read
    };
    Some(each(kind, paths(&operands)))
}

/// A sed script made only of commands that touch no file but the input: addresses, `p d q =`
/// and the like, `s///` with flags `g p i I m M` and digits, `y///`, `{ }`. `w`, `r`, `e`, `a`,
/// `i`, `c`, labels and branches fail it.
fn sed_script_safe(script: &str) -> bool {
    let s: Vec<char> = script.chars().collect();
    let mut i = 0;
    let n = s.len();
    let skip_ws = |i: &mut usize| {
        while *i < n && (s[*i].is_whitespace() || s[*i] == ';') {
            *i += 1;
        }
    };
    let delimited = |i: &mut usize, delim: char| -> bool {
        while *i < n {
            if s[*i] == '\\' {
                *i += 2;
                continue;
            }
            if s[*i] == delim {
                *i += 1;
                return true;
            }
            *i += 1;
        }
        false
    };
    let address = |i: &mut usize| -> bool {
        if *i < n && (s[*i].is_ascii_digit() || s[*i] == '$') {
            while *i < n && (s[*i].is_ascii_digit() || s[*i] == '$' || s[*i] == '~') {
                *i += 1;
            }
            return true;
        }
        if *i < n && s[*i] == '/' {
            *i += 1;
            if !delimited(i, '/') {
                return false;
            }
            while *i < n && (s[*i] == 'I' || s[*i] == 'M') {
                *i += 1;
            }
            return true;
        }
        if *i + 1 < n && s[*i] == '\\' {
            let d = s[*i + 1];
            *i += 2;
            return delimited(i, d);
        }
        true
    };
    loop {
        skip_ws(&mut i);
        if i >= n {
            return true;
        }
        if s[i] == '}' {
            i += 1;
            continue;
        }
        if !address(&mut i) {
            return false;
        }
        if i < n && s[i] == ',' {
            i += 1;
            if i < n && (s[i] == '+' || s[i] == '~') {
                i += 1;
            }
            if !address(&mut i) {
                return false;
            }
        }
        while i < n && (s[i] == ' ' || s[i] == '!') {
            i += 1;
        }
        if i >= n {
            return false;
        }
        let c = s[i];
        i += 1;
        match c {
            '{' => continue,
            'p' | 'P' | 'd' | 'D' | 'n' | 'N' | 'h' | 'H' | 'g' | 'G' | 'x' | 'z' | '=' | 'l' => {}
            'q' | 'Q' => {
                while i < n && s[i].is_ascii_digit() {
                    i += 1;
                }
            }
            's' | 'y' => {
                if i >= n {
                    return false;
                }
                let d = s[i];
                if d == '\\' || d == '\n' {
                    return false;
                }
                i += 1;
                if !delimited(&mut i, d) || !delimited(&mut i, d) {
                    return false;
                }
                if c == 's' {
                    while i < n && matches!(s[i], 'g' | 'p' | 'i' | 'I' | 'm' | 'M' | '0'..='9') {
                        i += 1;
                    }
                }
            }
            _ => return false,
        }
        // A command ends at `;`, a newline, `}` or the end.
        while i < n && s[i] == ' ' {
            i += 1;
        }
        if i < n && !matches!(s[i], ';' | '\n' | '}') {
            return false;
        }
    }
}

fn grep(args: &[Word], first: bool) -> Option<Found> {
    let (opts, mut ops) = options(
        args,
        &Spec {
            flags: "abcEFGHhIiLlnoPqRrsTUuvwxZz",
            valued: "ABCDdem",
            long: &[
                "count",
                "color",
                "colour",
                "files-with-matches",
                "files-without-match",
                "fixed-strings",
                "extended-regexp",
                "perl-regexp",
                "basic-regexp",
                "ignore-case",
                "no-ignore-case",
                "invert-match",
                "word-regexp",
                "line-regexp",
                "line-number",
                "no-filename",
                "with-filename",
                "only-matching",
                "quiet",
                "silent",
                "recursive",
                "dereference-recursive",
                "null",
                "null-data",
                "text",
                "no-messages",
                "byte-offset",
                "initial-tab",
            ],
            long_valued: &[
                "include",
                "exclude",
                "exclude-dir",
                "max-count",
                "after-context",
                "before-context",
                "context",
                "regexp",
                "binary-files",
                "devices",
                "directories",
            ],
            numeric: true,
        },
    )?;
    let explicit: Vec<&Word> = opts
        .iter()
        .filter(|(n, _)| n == "e" || n == "regexp")
        .filter_map(|(_, v)| *v)
        .collect();
    let pattern = if explicit.is_empty() {
        if ops.is_empty() {
            return None;
        }
        Some(ops.remove(0))
    } else {
        explicit.first().copied()
    };
    let label = pattern.and_then(|w| w.literal()).map(pattern_label);
    let recursive = opts.iter().any(|(n, _)| {
        matches!(
            n.as_str(),
            "r" | "R" | "recursive" | "dereference-recursive"
        )
    });
    // `-q` exits at the first match: only the first operand was certainly opened — and when
    // that one is stdin or an expansion, none that is a path here.
    if opts
        .iter()
        .any(|(n, _)| matches!(n.as_str(), "q" | "quiet" | "silent"))
    {
        ops.truncate(1);
    }
    let mut ps = paths(&ops);
    if ps.is_empty() && ops.is_empty() && recursive && first {
        ps.push(".".to_string());
    }
    Some(
        ps.into_iter()
            .map(|p| (StepKind::Search, p, label.clone()))
            .collect(),
    )
}

fn rg(args: &[Word], first: bool) -> Option<Found> {
    let (opts, mut ops) = options(
        args,
        &Spec {
            flags: "abcFHhiIlLnNoOpqsSuUvwxz0.",
            valued: "ABCegmMtTjdEr",
            long: &[
                "hidden",
                "no-ignore",
                "no-ignore-vcs",
                "follow",
                "files",
                "files-with-matches",
                "files-without-match",
                "count",
                "count-matches",
                "fixed-strings",
                "ignore-case",
                "smart-case",
                "case-sensitive",
                "invert-match",
                "word-regexp",
                "line-regexp",
                "line-number",
                "no-line-number",
                "no-filename",
                "with-filename",
                "only-matching",
                "quiet",
                "json",
                "vimgrep",
                "heading",
                "no-heading",
                "column",
                "multiline",
                "multiline-dotall",
                "pcre2",
                "null",
                "text",
                "trim",
                "stats",
                "no-messages",
                "color",
                "sort",
                "sortr",
                "unrestricted",
                "binary",
                "search-zip",
                "crlf",
                "passthru",
                "no-config",
                "glob-case-insensitive",
                "sort-files",
                "byte-offset",
                "pretty",
            ],
            long_valued: &[
                "glob",
                "iglob",
                "type",
                "type-not",
                "regexp",
                "max-count",
                "max-depth",
                "max-filesize",
                "after-context",
                "before-context",
                "context",
                "replace",
                "max-columns",
                "threads",
                "colors",
                "encoding",
                "engine",
            ],
            numeric: false,
        },
    )?;
    // `-q` stops at the first match, and rg searches in parallel: no operand is certain.
    if opts.iter().any(|(n, _)| n == "q" || n == "quiet") {
        return None;
    }
    let files_mode = opts.iter().any(|(n, _)| n == "files");
    let explicit: Vec<&Word> = opts
        .iter()
        .filter(|(n, _)| n == "e" || n == "regexp")
        .filter_map(|(_, v)| *v)
        .collect();
    let label = if files_mode {
        None
    } else if explicit.is_empty() {
        if ops.is_empty() {
            return None;
        }
        Some(ops.remove(0))
            .and_then(|w| w.literal())
            .map(pattern_label)
    } else {
        explicit[0].literal().map(pattern_label)
    };
    let mut ps = paths(&ops);
    if ps.is_empty() && ops.is_empty() && first {
        ps.push(".".to_string());
    }
    Some(
        ps.into_iter()
            .map(|p| (StepKind::Search, p, label.clone()))
            .collect(),
    )
}

fn find(args: &[Word]) -> Option<Found> {
    let mut i = 0;
    while i < args.len() && matches!(args[i].text.as_str(), "-L" | "-H" | "-P") {
        i += 1;
    }
    let mut roots = Vec::new();
    while i < args.len() {
        let t = args[i].text.as_str();
        if t.starts_with('-') || t == "(" || t == "!" {
            break;
        }
        roots.push(&args[i]);
        i += 1;
    }
    let mut label = None;
    while i < args.len() {
        let w = &args[i];
        i += 1;
        if !w.literal {
            return None;
        }
        match w.text.as_str() {
            "-name" | "-iname" | "-path" | "-ipath" | "-regex" | "-iregex" => {
                let v = args.get(i)?;
                i += 1;
                if label.is_none() {
                    label = Some(v.text.clone());
                }
            }
            "-type" | "-maxdepth" | "-mindepth" | "-mtime" | "-mmin" | "-newer" | "-size" => {
                args.get(i)?;
                i += 1;
            }
            "-empty" | "-prune" | "-print" | "-print0" | "-o" | "-or" | "-a" | "-and" | "-not"
            | "!" | "(" | ")" => {}
            _ => return None,
        }
    }
    let mut ps = paths(&roots);
    if roots.is_empty() {
        ps.push(".".to_string());
    }
    Some(
        ps.into_iter()
            .map(|p| (StepKind::Search, p, label.clone()))
            .collect(),
    )
}

fn pattern_label(p: &str) -> String {
    p.lines().next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = "/r";
    const HOME: &str = "/h";

    fn fx(script: &str) -> Vec<(StepKind, String, Outcome)> {
        analyze(script, CWD, HOME, false)
            .effects
            .into_iter()
            .map(|e| (e.kind, e.path, e.on_ok))
            .collect()
    }

    fn opaque(script: &str) -> bool {
        analyze(script, CWD, HOME, false).opaque
    }

    use Outcome::{Ok as O, Unknown as U};
    use StepKind::{Delete as D, Edit as E, Read as R, Search as S, Write as W};

    fn e(k: StepKind, p: &str, o: Outcome) -> (StepKind, String, Outcome) {
        (k, p.to_string(), o)
    }

    #[test]
    fn reads_of_the_shapes_agents_use() {
        assert_eq!(fx("sed -n 1,60p src/a.rs"), vec![e(R, "/r/src/a.rs", O)]);
        assert_eq!(
            fx("sed -n '1530,1580p' docs/t.md"),
            vec![e(R, "/r/docs/t.md", O)]
        );
        assert_eq!(
            fx("cat a.rs b.rs"),
            vec![e(R, "/r/a.rs", O), e(R, "/r/b.rs", O)]
        );
        assert_eq!(fx("head -40 a"), vec![e(R, "/r/a", O)]);
        assert_eq!(fx("head -n 40 a"), vec![e(R, "/r/a", O)]);
        assert_eq!(fx("tail -n +5 /abs/f.log"), vec![e(R, "/abs/f.log", O)]);
        assert_eq!(fx("wc -l a b"), vec![e(R, "/r/a", O), e(R, "/r/b", O)]);
        assert_eq!(fx("nl -ba src/x.rs"), vec![e(R, "/r/src/x.rs", O)]);
        assert_eq!(fx("cat ~/notes.md"), vec![e(R, "/h/notes.md", O)]);
        assert_eq!(fx("cat 'with space.md'"), vec![e(R, "/r/with space.md", O)]);
        assert_eq!(fx("cat ../x/./y"), vec![e(R, "/x/y", O)]);
        assert_eq!(
            fx("sed -n '1,9p' a.rs\nsed -n '20,30p' b.rs"),
            vec![e(R, "/r/a.rs", U), e(R, "/r/b.rs", O)]
        );
    }

    #[test]
    fn a_pipeline_proves_only_its_last_stage() {
        assert_eq!(
            fx("cat a | head -5"),
            vec![e(R, "/r/a", U)],
            "head read the pipe, cat's success is unknown"
        );
        assert_eq!(fx("sed -n 1,9p a | grep foo"), vec![e(R, "/r/a", U)]);
        assert!(!opaque("cat a | head -5"));
    }

    #[test]
    fn searches_carry_their_pattern() {
        let a = analyze("grep -n 'fn main' src/a.rs src/b.rs", CWD, HOME, false);
        assert_eq!(a.effects.len(), 2);
        assert!(
            a.effects
                .iter()
                .all(|x| x.kind == S && x.label.as_deref() == Some("fn main"))
        );
        assert_eq!(fx("grep -rn foo"), vec![e(S, "/r", O)]);
        assert_eq!(fx("rg -n pat -g '*.md' docs"), vec![e(S, "/r/docs", O)]);
        assert_eq!(
            fx("rg -n pat"),
            vec![e(S, "/r", O)],
            "rg with no path searches the cwd"
        );
        assert_eq!(fx("rg --files src"), vec![e(S, "/r/src", O)]);
        assert_eq!(fx("grep -e a -e b x"), vec![e(S, "/r/x", O)]);
        assert_eq!(fx("grep -C3 foo x"), vec![e(S, "/r/x", O)]);
        assert_eq!(
            fx("grep --include=*.rs -rn foo src"),
            vec![e(S, "/r/src", O)]
        );
        assert_eq!(fx("find src -name '*.rs' -type f"), vec![e(S, "/r/src", O)]);
        assert_eq!(
            fx("find src \\( -name a -o -name b \\)"),
            vec![e(S, "/r/src", O)]
        );
        assert_eq!(fx("ls src"), vec![e(S, "/r/src", O)]);
        // A grep reading a pipe searches no file, and is still understood.
        assert_eq!(fx("git log | grep foo"), vec![]);
        assert!(opaque("git log | grep foo"), "git is not");
        assert_eq!(
            fx("cat a | rg foo"),
            vec![e(R, "/r/a", U)],
            "rg reads the pipe"
        );
    }

    #[test]
    fn edits_writes_and_deletes() {
        assert_eq!(fx("sed -i 's/a/b/' f"), vec![e(E, "/r/f", O)]);
        assert_eq!(
            fx("sed -i '' 's/a/b/g' f"),
            vec![e(E, "/r/f", O)],
            "BSD empty suffix"
        );
        assert_eq!(
            fx("sed -i.bak -e 's/a/b/' f g"),
            vec![e(E, "/r/f", O), e(E, "/r/g", O)]
        );
        assert_eq!(fx("sed --in-place -e 's|x|y|' f"), vec![e(E, "/r/f", O)]);
        assert_eq!(fx("echo hi > out.txt"), vec![e(W, "/r/out.txt", O)]);
        assert_eq!(fx("echo hi >> log.txt"), vec![e(E, "/r/log.txt", O)]);
        assert_eq!(fx("cargo test 2>&1 > /dev/null"), vec![]);
        assert_eq!(fx("cargo test > out 2>&1"), vec![e(W, "/r/out", O)]);
        assert_eq!(fx("cargo test 2>err.log"), vec![e(W, "/r/err.log", O)]);
        assert_eq!(fx("python3 x.py > out.txt"), vec![e(W, "/r/out.txt", O)]);
        assert!(opaque("python3 x.py > out.txt"));
        assert_eq!(
            fx("printf x | tee a b"),
            vec![e(W, "/r/a", O), e(W, "/r/b", O)]
        );
        assert_eq!(fx("printf x | tee -a a"), vec![e(E, "/r/a", O)]);
        assert_eq!(fx("rm -f a b"), vec![e(D, "/r/a", O), e(D, "/r/b", O)]);
        assert_eq!(fx("cat < in.txt"), vec![e(R, "/r/in.txt", O)]);
    }

    #[test]
    fn heredocs_are_data_not_commands() {
        let s = "cat > /tmp/x.py <<'PY'\nimport os\nrm -rf /\nPY\ncat b";
        assert_eq!(fx(s), vec![e(W, "/tmp/x.py", U), e(R, "/r/b", O)]);
        let two = "cat > a <<EOF\nrm x\nEOF\ncat > b <<-EOF\n\trm y\n\tEOF";
        assert_eq!(fx(two), vec![e(W, "/r/a", U), e(W, "/r/b", O)]);
        assert_eq!(fx("python3 - <<'PY'\nopen('x','w')\nPY"), vec![]);
        assert!(opaque("python3 - <<'PY'\nopen('x','w')\nPY"));
        assert_eq!(
            fx("cat <<EOF\nnever ends"),
            vec![],
            "an unterminated heredoc stops"
        );
        assert_eq!(fx("grep -c x <<< \"$v\""), vec![]);
    }

    #[test]
    fn chains_and_cwd() {
        assert_eq!(fx("cd /p && sed -n 1p a"), vec![e(R, "/p/a", O)]);
        assert_eq!(
            fx("cd sub && cat a && cat b"),
            vec![e(R, "/r/sub/a", O), e(R, "/r/sub/b", O)]
        );
        // After the chain that held a `cd`, the directory is unknown: relative is dropped.
        assert_eq!(fx("cd sub && true; cat a; cat /abs"), vec![e(R, "/abs", O)]);
        assert_eq!(fx("cd missing; cat a"), vec![]);
        // Already there: nothing moves, so the directory stays known.
        assert_eq!(fx("cd /r; cat a"), vec![e(R, "/r/a", O)]);
        assert_eq!(fx("cd /r/sub/..; cat a"), vec![e(R, "/r/a", O)]);
        assert_eq!(fx("cd sub | cat a"), vec![]);
        assert_eq!(fx("printf x | cd sub && cat a"), vec![]);
        assert_eq!(fx("eval 'cd /t'; cat a"), vec![]);
        assert_eq!(fx(". ./env.sh; cat a"), vec![]);
        assert_eq!(fx("command cd sub && cat a"), vec![]);
        assert_eq!(fx("cd $D && cat a"), vec![]);
        // `false && rm a; true`: rm may never have run.
        assert_eq!(fx("false && rm a; true"), vec![]);
        assert_eq!(fx("sed -i 's/a/b/' a; true"), vec![e(E, "/r/a", U)]);
        assert_eq!(fx("sed -i 's/a/b/' a && true"), vec![e(E, "/r/a", O)]);
    }

    #[test]
    fn what_stops_the_parse() {
        // `||`: the prefix's success is no longer proven.
        assert_eq!(
            fx("sed -i 's/a/b/' missing || true"),
            vec![e(E, "/r/missing", U)]
        );
        assert_eq!(fx("true || rm a"), vec![]);
        assert!(opaque("true || rm a"));
        assert_eq!(
            fx("cat < /missing > out; true"),
            vec![e(R, "/missing", U), e(W, "/r/out", U)]
        );
        assert_eq!(fx("(cd sub; cat a); cat b"), vec![]);
        assert_eq!(fx("sleep 5 & cat a"), vec![]);
        assert_eq!(fx("diff <(cat a) <(cat b)"), vec![]);
        assert_eq!(fx("cat $(ls)"), vec![]);
        assert_eq!(fx("cat `ls`"), vec![]);
        assert_eq!(fx("for f in a b; do cat $f; done"), vec![]);
        assert_eq!(fx("if true; then rm a; fi"), vec![]);
        assert_eq!(fx("cat a; { rm b; }"), vec![e(R, "/r/a", U)]);
        assert_eq!(fx("f() { rm a; }; f"), vec![]);
        assert_eq!(fx("cat 'unterminated"), vec![]);
        assert!(analyze("cat a", CWD, HOME, true).opaque);
        assert_eq!(
            analyze("cat a && rm b", CWD, HOME, true)
                .effects
                .iter()
                .map(|x| x.on_ok)
                .collect::<Vec<_>>(),
            vec![U],
            "a background call proves nothing; only the chain's head certainly ran"
        );
    }

    #[test]
    fn only_literal_operands_are_paths() {
        assert_eq!(fx("cat $F"), vec![]);
        assert_eq!(fx("cat \"$F/x\""), vec![]);
        assert_eq!(fx("cat *.rs"), vec![]);
        assert_eq!(fx("cat a{1,2}"), vec![]);
        assert_eq!(fx("cat ~bob/x"), vec![]);
        assert_eq!(
            fx("cat '*.rs'"),
            vec![e(R, "/r/*.rs", O)],
            "quoted is literal"
        );
        assert_eq!(fx("rm -- -weird"), vec![e(D, "/r/-weird", O)]);
        assert_eq!(fx("X=1 Y=2 cat a"), vec![e(R, "/r/a", O)]);
    }

    #[test]
    fn options_outside_the_table_make_the_stage_opaque() {
        assert_eq!(fx("rm --help victim"), vec![]);
        assert!(opaque("rm --help victim"));
        assert_eq!(
            fx("rm -rf build"),
            vec![],
            "a recursive rm names a directory"
        );
        assert_eq!(fx("cat --version a"), vec![]);
        assert_eq!(fx("grep -f pats x"), vec![]);
        assert_eq!(fx("sudo rm a"), vec![]);
        assert_eq!(fx("sudo -u u rm a"), vec![]);
        assert_eq!(fx("env -C /tmp rm a"), vec![]);
        assert_eq!(fx("nice -n 10 rm a"), vec![]);
        assert_eq!(fx("command -v rm"), vec![]);
        assert_eq!(fx("xargs cat < list"), vec![e(R, "/r/list", O)]);
        assert!(opaque("xargs cat < list"));
        assert_eq!(fx("find src -delete"), vec![]);
        assert_eq!(fx("find . -name x -exec rm {} +"), vec![]);
    }

    #[test]
    fn what_a_successful_exit_does_not_prove() {
        // A quoted `~` is not expanded.
        assert_eq!(fx("cat '~/notes.md'"), vec![e(R, "/r/~/notes.md", O)]);
        assert_eq!(fx("echo x > '~/o'"), vec![e(W, "/r/~/o", O)]);
        // A cd's own redirection is opened where the shell was.
        assert_eq!(fx("cd sub > out"), vec![e(W, "/r/out", O)]);
        // A declined prompt still exits 0.
        assert_eq!(fx("rm -i a"), vec![]);
        assert_eq!(fx("rm -I a b"), vec![]);
        // `-q` exits at the first match; rg also searches in parallel.
        assert_eq!(fx("grep -q needle a b"), vec![e(S, "/r/a", O)]);
        assert_eq!(fx("grep -q needle - b"), vec![]);
        assert_eq!(fx("grep -q needle \"$F\" b"), vec![]);
        assert_eq!(fx("rg -q needle a b"), vec![]);
    }

    #[test]
    fn sed_scripts_that_write_are_opaque() {
        assert_eq!(fx("sed -n 'w out' input"), vec![]);
        assert_eq!(fx("sed 's/x/y/w out' input"), vec![]);
        assert_eq!(fx("sed '1e date' input"), vec![]);
        assert_eq!(fx("sed '/x/r other' input"), vec![]);
        assert_eq!(fx("sed '1i hello' input"), vec![]);
        assert_eq!(fx("sed -f prog.sed input"), vec![]);
        assert!(sed_script_safe("1,60p"));
        assert!(sed_script_safe("5p;10p"));
        assert!(sed_script_safe("/^fn /,/^}/p"));
        assert!(sed_script_safe("$p"));
        assert!(sed_script_safe("10q"));
        assert!(sed_script_safe("s/a\\/b/c/g"));
        assert!(sed_script_safe("s|a|b|; s#c#d#2"));
        assert!(sed_script_safe("/x/!d"));
        assert!(sed_script_safe("1~2{p}"));
        assert!(!sed_script_safe("s/a/b/e"));
        assert!(!sed_script_safe("s/a/b"));
    }

    #[test]
    fn comments_and_remote_commands_are_not_parsed() {
        assert_eq!(fx("cat a # rm b"), vec![e(R, "/r/a", O)]);
        assert_eq!(fx("ssh host \"sed -i 's/a/b/' f\""), vec![]);
        assert_eq!(fx("echo 'rm a; cat b'"), vec![]);
        assert!(!opaque("echo done"));
    }

    #[test]
    fn unknown_directory_drops_relative_but_keeps_absolute() {
        let a = analyze("cat a /abs", "", HOME, false);
        assert_eq!(
            a.effects
                .iter()
                .map(|x| x.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/abs"]
        );
    }
}
