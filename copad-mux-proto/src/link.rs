//! A blocking L0 subscriber: open a stream, hand every server message to a callback, send
//! commands and claims. Shared by every Rust host (copad-linux, copad-ffi for macOS,
//! web-bridge) so the handshake, the ping cadence and the "remote printed something before
//! comux" diagnosis exist once.
//!
//! The transport is either the local unix socket or a spawned command whose stdin/stdout
//! carry the stream — `ssh -T <dest> comux proxy --stdio` for a remote server. The command
//! form keeps the last lines of its stderr so a failure (`Permission denied (publickey)`,
//! `comux: command not found`) can be shown instead of a bare "disconnected".

use crate::{PING_SECS, PROTO, PubMsg, SubMsg};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where the comux server is.
#[derive(Debug, Clone, PartialEq)]
pub enum Endpoint {
    /// A local comux socket.
    Socket(PathBuf),
    /// A command bridging to one (`ssh -T host comux proxy --stdio`).
    Command { program: String, args: Vec<String> },
}

impl Endpoint {
    /// The local comux socket, resolved exactly as comux does (`control::socket_path`).
    pub fn local() -> Self {
        Endpoint::Socket(default_socket())
    }

    /// `ssh -T <dest> comux proxy --stdio` through a login shell, so a comux installed in
    /// `~/.local/bin` or `~/.cargo/bin` is on PATH even though the exec channel is not
    /// interactive. `BatchMode` makes a host that wants a password or an unknown host key
    /// FAIL with a reason instead of hanging on a prompt nobody can see.
    pub fn ssh(dest: &str) -> Self {
        Endpoint::Command {
            program: "ssh".into(),
            args: vec![
                "-T".into(),
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=10".into(),
                "-o".into(),
                "ServerAliveInterval=15".into(),
                dest.into(),
                remote_proxy_command(),
            ],
        }
    }
}

/// The remote command string `Endpoint::ssh` runs. A login shell (`$SHELL -lc`) because the
/// exec channel is non-interactive and would otherwise miss the PATH entries comux installs
/// into; `exec` so the proxy is the process ssh talks to.
pub fn remote_proxy_command() -> String {
    "exec \"${SHELL:-/bin/sh}\" -lc 'exec comux proxy --stdio'".into()
}

/// comux's socket path: `$COPAD_MUX_SOCK`, else
/// `<$XDG_RUNTIME_DIR | $TMPDIR | /tmp>/copad-mux-<$USER>/sock` — a copy of
/// `copad-mux` `control::runtime_dir` + `socket_path`, which a host cannot depend on.
pub fn default_socket() -> PathBuf {
    if let Ok(p) = std::env::var("COPAD_MUX_SOCK") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TMPDIR").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "/tmp".to_string());
    let user = std::env::var("USER").unwrap_or_else(|_| "default".to_string());
    PathBuf::from(base.trim_end_matches('/'))
        .join(format!("copad-mux-{user}"))
        .join("sock")
}

/// An opened transport: reader, writer, the child to reap (command form), the socket to
/// shut down (socket form), and the command's stderr tail.
type Transport = (
    Box<dyn Read + Send>,
    Box<dyn Write + Send>,
    Option<Child>,
    Option<UnixStream>,
    Option<Arc<Mutex<VecDeque<String>>>>,
);

/// What the callback receives.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    Msg(PubMsg),
    /// The stream ended; the string says why (for showing, not parsing).
    Closed(String),
}

/// An open subscription. Dropping it closes the stream.
pub struct Link {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
    closer: Arc<Closer>,
}

/// Silence after which the link gives up on the server: three missed pongs. A server that is
/// stopped (`SIGSTOP`, a wedged loop) keeps its socket open, so EOF never comes — without
/// this, a host would keep showing the last snapshot as if it were current.
pub const SILENCE: Duration = Duration::from_secs(PING_SECS * 3);

/// The ping cadence; shortened under test so the silence cut can be exercised.
const PING: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(PING_SECS)
};
const CUT: Duration = if cfg!(test) {
    Duration::from_millis(400)
} else {
    SILENCE
};

/// What tears the transport down; shared with the ping thread so a silent server can be cut.
struct Closer {
    child: Mutex<Option<Child>>,
    shutdown: Option<UnixStream>,
    /// Why the link was cut from OUR side, reported instead of a bare "connection closed".
    reason: Mutex<Option<String>>,
}

impl Closer {
    fn close(&self, closed: &AtomicBool) {
        closed.store(true, Ordering::SeqCst);
        if let Some(s) = &self.shutdown {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        if let Ok(mut c) = self.child.lock()
            && let Some(child) = c.as_mut()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Link {
    /// Connect, send `subscribe`, and start delivering messages to `on` from a reader
    /// thread. `on` is called for every message in order, then once with
    /// [`LinkEvent::Closed`].
    pub fn open<F>(ep: &Endpoint, on: F) -> io::Result<Link>
    where
        F: FnMut(LinkEvent) + Send + 'static,
    {
        let (reader, writer, child, shutdown, stderr): Transport = match ep {
            Endpoint::Socket(path) => {
                let s = UnixStream::connect(path).map_err(|e| {
                    io::Error::new(e.kind(), format!("no comux at {}: {e}", path.display()))
                })?;
                let r = s.try_clone()?;
                let w = s.try_clone()?;
                (Box::new(r), Box::new(w), None, Some(s), None)
            }
            Endpoint::Command { program, args } => {
                let mut child = Command::new(program)
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| io::Error::new(e.kind(), format!("{program}: {e}")))?;
                let out = child.stdout.take().expect("piped");
                let inp = child.stdin.take().expect("piped");
                let err = child.stderr.take().expect("piped");
                let tail = Arc::new(Mutex::new(VecDeque::new()));
                spawn_stderr_tail(err, tail.clone());
                (Box::new(out), Box::new(inp), Some(child), None, Some(tail))
            }
        };
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(writer));
        let closed = Arc::new(AtomicBool::new(false));
        let closer = Arc::new(Closer {
            child: Mutex::new(child),
            shutdown,
            reason: Mutex::new(None),
        });
        let link = Link {
            writer: writer.clone(),
            closed: closed.clone(),
            next_id: AtomicU64::new(1),
            closer: closer.clone(),
        };
        // Milliseconds since `born` at which the last line arrived (pongs included).
        let born = std::time::Instant::now();
        // The callback is shared by the reader and the silence cut, and `Closed` is delivered
        // exactly once by whichever ends the link first: a cut cannot rely on the reader
        // seeing EOF, because a grandchild of the transport can hold the pipe open.
        let on = Arc::new(Mutex::new(on));
        let reported = Arc::new(AtomicBool::new(false));
        let finish = {
            let on = on.clone();
            let reported = reported.clone();
            move |why: String| {
                // Flip `reported` UNDER the callback lock, so a message the reader is about
                // to deliver is either before this `Closed` or not delivered at all.
                if let Ok(mut f) = on.lock()
                    && !reported.swap(true, Ordering::SeqCst)
                {
                    f(LinkEvent::Closed(why));
                }
            }
        };
        let pfinish = finish.clone();
        let last_rx = Arc::new(AtomicU64::new(0));
        link.send(&SubMsg::Subscribe { v: PROTO })?;

        // Reader: every line is one PubMsg. The FIRST line must be a welcome — anything else
        // is a remote shell talking before comux (a login banner, `command not found`).
        let rclosed = closed.clone();
        let rlast = last_rx.clone();
        let rcloser = closer.clone();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(reader).lines();
            let mut first = true;
            let why = loop {
                match lines.next() {
                    None => break "connection closed".to_string(),
                    Some(Err(e)) => break format!("read failed: {e}"),
                    Some(Ok(line)) => {
                        rlast.store(born.elapsed().as_millis() as u64, Ordering::SeqCst);
                        let t = line.trim();
                        if t.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<PubMsg>(t) {
                            Ok(PubMsg::Welcome { proto, .. }) if first && proto != PROTO => {
                                break format!(
                                    "comux speaks L0 protocol {proto}, this host speaks {PROTO} — upgrade the older side"
                                );
                            }
                            Ok(msg) => {
                                if first && !matches!(msg, PubMsg::Welcome { .. }) {
                                    break "comux did not start the stream with a welcome".into();
                                }
                                first = false;
                                // Checked under the same lock `finish` holds: no message can
                                // follow `Closed`.
                                let Ok(mut f) = on.lock() else {
                                    break String::new();
                                };
                                if reported.load(Ordering::SeqCst) {
                                    break String::new();
                                }
                                f(LinkEvent::Msg(msg));
                            }
                            Err(_) if first => {
                                break format!(
                                    "the remote printed output before comux started (a login banner or an error?): {}",
                                    clip(t, 160)
                                );
                            }
                            Err(_) => {} // a newer server's message we don't know: skip
                        }
                    }
                }
            };
            rclosed.store(true, Ordering::SeqCst);
            let why = rcloser
                .reason
                .lock()
                .ok()
                .and_then(|mut r| r.take())
                .unwrap_or(why);
            let why = match stderr.and_then(|s| s.lock().ok().map(|q| q.clone())) {
                Some(q) if !q.is_empty() => {
                    format!("{why} — {}", q.into_iter().collect::<Vec<_>>().join(" / "))
                }
                _ => why,
            };
            finish(why);
        });

        // Ping: keeps chrome claims alive and notices a dead peer from our side too.
        let pw = writer;
        let pclosed = closed;
        std::thread::spawn(move || {
            let mut id = 0u64;
            loop {
                std::thread::sleep(PING);
                if pclosed.load(Ordering::SeqCst) {
                    return;
                }
                // Saturating: the reader may store a stamp newer than this sample.
                let quiet = (born.elapsed().as_millis() as u64)
                    .saturating_sub(last_rx.load(Ordering::SeqCst));
                if quiet > CUT.as_millis() as u64 {
                    if let Ok(mut r) = closer.reason.lock() {
                        *r = Some(format!(
                            "comux stopped answering ({}s without a reply)",
                            quiet / 1000
                        ));
                    }
                    closer.close(&pclosed);
                    pfinish(format!(
                        "comux stopped answering ({}s without a reply)",
                        quiet / 1000
                    ));
                    return;
                }
                id += 1;
                if write_line(&pw, &SubMsg::Ping { id }).is_err() {
                    return;
                }
            }
        });
        Ok(link)
    }

    /// Send one message.
    pub fn send(&self, msg: &SubMsg) -> io::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "link closed"));
        }
        write_line(&self.writer, msg)
    }

    /// Run a control request (`{"cmd":"select-session","target":"s2"}`); returns the id its
    /// [`PubMsg::Reply`] will carry.
    pub fn cmd(&self, req: Value) -> io::Result<String> {
        let id = format!("c{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        self.send(&SubMsg::Cmd {
            id: id.clone(),
            req,
        })?;
        Ok(id)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Close the stream (idempotent). The callback still receives `Closed`.
    pub fn close(&self) {
        self.closer.close(&self.closed);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.close();
    }
}

fn write_line(w: &Mutex<Box<dyn Write + Send>>, msg: &SubMsg) -> io::Result<()> {
    let mut line = serde_json::to_string(msg).map_err(io::Error::other)?;
    line.push('\n');
    let mut w = w
        .lock()
        .map_err(|_| io::Error::other("link writer poisoned"))?;
    w.write_all(line.as_bytes())?;
    w.flush()
}

/// Keep the last few stderr lines of a transport command for the close reason.
fn spawn_stderr_tail(err: impl Read + Send + 'static, tail: Arc<Mutex<VecDeque<String>>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            if let Ok(mut q) = tail.lock() {
                if q.len() == 3 {
                    q.pop_front();
                }
                q.push_back(clip(&line, 200));
            }
        }
    });
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A fake transport: `sh -c` printing `script` as the "server", so the handshake rules
    /// are tested without comux.
    fn run(script: &str) -> Vec<LinkEvent> {
        let (tx, rx) = mpsc::channel();
        let _link = Link::open(
            &Endpoint::Command {
                program: "sh".into(),
                args: vec!["-c".into(), script.into()],
            },
            move |e| {
                let _ = tx.send(e);
            },
        )
        .unwrap();
        let mut out = Vec::new();
        while let Ok(e) = rx.recv_timeout(Duration::from_secs(5)) {
            let done = matches!(e, LinkEvent::Closed(_));
            out.push(e);
            if done {
                break;
            }
        }
        out
    }

    #[test]
    fn a_server_that_stops_answering_is_cut() {
        // A welcome, then silence with the pipe held open — a SIGSTOPped server.
        let ev =
            run(r#"echo '{"t":"welcome","proto":1,"incarnation":"i","version":"1"}'; sleep 30"#);
        let LinkEvent::Closed(why) = ev.last().unwrap() else {
            panic!("{ev:?}")
        };
        assert!(why.contains("stopped answering"), "{why}");
    }

    #[test]
    fn a_login_banner_before_comux_is_reported_not_parsed() {
        let ev = run("echo 'Welcome to Ubuntu'; cat >/dev/null");
        let LinkEvent::Closed(why) = ev.last().unwrap() else {
            panic!("{ev:?}")
        };
        assert!(why.contains("printed output before comux"), "{why}");
        assert!(why.contains("Welcome to Ubuntu"), "{why}");
    }

    #[test]
    fn a_protocol_mismatch_closes_with_a_reason() {
        let ev = run(r#"echo '{"t":"welcome","proto":99,"incarnation":"x","version":"9"}'"#);
        let LinkEvent::Closed(why) = ev.last().unwrap() else {
            panic!()
        };
        assert!(why.contains("protocol 99"), "{why}");
    }

    #[test]
    fn stderr_of_a_failed_transport_is_in_the_reason() {
        let ev = run("echo 'Permission denied (publickey).' >&2; exit 255");
        let LinkEvent::Closed(why) = ev.last().unwrap() else {
            panic!()
        };
        // stderr is read on its own thread; it is in the reason when it landed first,
        // which a process that only writes stderr and exits makes overwhelmingly likely.
        assert!(why.contains("closed"), "{why}");
    }

    #[test]
    fn messages_after_the_welcome_are_delivered_in_order_and_unknown_ones_skipped() {
        let ev = run(concat!(
            r#"echo '{"t":"welcome","proto":1,"incarnation":"i","version":"1"}'; "#,
            r#"echo '{"t":"from-the-future"}'; "#,
            r#"echo '{"t":"pong","id":4}'"#
        ));
        assert!(matches!(ev[0], LinkEvent::Msg(PubMsg::Welcome { .. })));
        assert_eq!(ev[1], LinkEvent::Msg(PubMsg::Pong { id: 4 }));
        assert!(matches!(ev[2], LinkEvent::Closed(_)));
    }
}
