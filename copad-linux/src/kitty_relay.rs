//! The kitty keyboard protocol for a VTE terminal (decisions #164).
//!
//! VTE does not implement the protocol (VTE issue #2601) and gives no hook into the output it
//! parses, so copad puts a second PTY between the shell and the widget:
//!
//! ```text
//!   shell ── P (its own PTY) ── [output worker: Tracker] ── R (raw PTY) ── VTE
//!                          ╰──── [input worker] ◄───────────────────────────╯
//! ```
//!
//! The output worker passes everything the shell writes to VTE through a
//! [`copad_keys::Tracker`], which takes out the negotiation sequences, keeps the flags, and
//! produces the answer to `CSI ? u`; the input worker carries VTE's writes (keys, pastes, its
//! own replies) and those answers to the shell. VTE keeps its own reader, writer, mouse and
//! sizing on R; copad copies R's size to P.
//!
//! Both workers use nonblocking descriptors and `poll`, each with a bounded buffer: a full
//! buffer stops reading its source (backpressure) instead of growing, so a shell that stops
//! reading its input cannot wedge the output side, and VTE pausing its reads slows the shell
//! down rather than queueing without bound. Dropping the [`Relay`] closes the write end of a
//! shared pipe both workers watch, which stops them whatever they are waiting on.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The most bytes either worker holds for a destination that is not taking them.
const BUFFER_CAP: usize = 64 * 1024;
/// The most answer bytes waiting for the input worker; more are dropped (a program flooding
/// queries while not reading its input loses answers, not memory).
const REPLY_CAP: usize = 4096;
/// After the shell exits, output from processes still holding its terminal is read until it
/// has been quiet this long…
const DRAIN_QUIET: Duration = Duration::from_millis(50);
/// …or for at most this long.
const DRAIN_MAX: Duration = Duration::from_secs(1);

/// State the workers publish for the key handler on the main thread.
#[derive(Default)]
struct Shared {
    flags: AtomicU8,
    app_cursor: AtomicBool,
    /// Set by the main thread when the shell's exit status arrived.
    child_exited: AtomicBool,
    /// Set by the output worker when it has delivered everything and closed its side of R.
    output_done: AtomicBool,
    replies: Mutex<VecDeque<u8>>,
}

pub struct Relay {
    shared: Arc<Shared>,
    /// Write end of the stop pipe. Dropping it is the stop signal.
    _stop: OwnedFd,
    wake_output: OwnedFd,
    /// The shell's PTY, kept on the main thread for `TIOCSWINSZ`.
    p_master: OwnedFd,
    size: std::cell::Cell<(u16, u16)>,
}

/// A raw PTY pair for VTE: `(master for VtePty::foreign_sync, slave for the workers)`.
pub fn open_raw_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut m, mut s) = (-1, -1);
    // SAFETY: openpty writes two descriptors; the name/termios/winsize pointers may be null.
    if unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded, so both are fresh descriptors we now own.
    let (m, s) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    set_cloexec(m.as_raw_fd())?;
    set_cloexec(s.as_raw_fd())?;
    // Raw: no echo, no CR/LF translation, no signals — bytes pass exactly.
    // SAFETY: termios is plain data; tcgetattr fills it for a valid tty descriptor.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(s.as_raw_fd(), &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut t);
        if libc::tcsetattr(s.as_raw_fd(), libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((m, s))
}

impl Relay {
    /// Start relaying between the shell's PTY master `p_master` and `r_slave`, the far end of
    /// the PTY VTE reads.
    pub fn start(p_master: BorrowedFd, r_slave: OwnedFd) -> io::Result<Relay> {
        // A VtePty is opened in packet mode (TIOCPKT: every read starts with a status byte),
        // which only VTE's own reader expects. The relay reads plain bytes.
        let off: libc::c_int = 0;
        // SAFETY: TIOCPKT reads an int from a valid pointer.
        if unsafe { libc::ioctl(p_master.as_raw_fd(), libc::TIOCPKT, &off) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let p_main = p_master.try_clone_to_owned()?;
        let p_out = p_master.try_clone_to_owned()?;
        let p_in = p_master.try_clone_to_owned()?;
        let r_in = r_slave.try_clone()?;
        let r_out = r_slave;
        for fd in [&p_out, &r_out] {
            // Dups share the file status flags, so this covers the input worker's copies.
            set_nonblocking(fd.as_raw_fd())?;
        }
        let (stop_r, stop_w) = pipe()?;
        let stop_r2 = stop_r.try_clone()?;
        let (wake_out_r, wake_out_w) = pipe()?;
        let (wake_in_r, wake_in_w) = pipe()?;
        let shared = Arc::new(Shared::default());

        let sh = shared.clone();
        std::thread::Builder::new()
            .name("kitty-relay-out".into())
            .spawn(move || {
                output_worker(OutputIo {
                    p: p_out,
                    r: r_out,
                    stop: stop_r,
                    wake: wake_out_r,
                    wake_input: wake_in_w,
                    shared: sh,
                })
            })?;
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("kitty-relay-in".into())
            .spawn(move || input_worker(r_in, p_in, stop_r2, wake_in_r, sh))?;
        Ok(Relay {
            shared,
            _stop: stop_w,
            wake_output: wake_out_w,
            p_master: p_main,
            size: std::cell::Cell::new((0, 0)),
        })
    }

    /// The flags the program negotiated on the screen that is up.
    pub fn flags(&self) -> u8 {
        self.shared.flags.load(Ordering::Acquire)
    }

    /// DECCKM, for the legacy cursor-key forms.
    pub fn app_cursor(&self) -> bool {
        self.shared.app_cursor.load(Ordering::Acquire)
    }

    /// The shell exited: read what its descendants still write for a moment, deliver it, then
    /// close VTE's side so VTE reaches EOF.
    pub fn child_exited(&self) {
        self.shared.child_exited.store(true, Ordering::Release);
        wake(self.wake_output.as_raw_fd());
    }

    /// Give the shell the size VTE laid out. No-op when unchanged.
    pub fn set_size(&self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 || self.size.get() == (rows, cols) {
            return;
        }
        self.size.set((rows, cols));
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads a winsize from a valid pointer.
        unsafe { libc::ioctl(self.p_master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// `(read end, write end)`, both nonblocking and close-on-exec.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: pipe2 writes two descriptors into the array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded; we own both.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn wake(fd: RawFd) {
    // A full pipe already means "wake up"; nothing else can go wrong worth reporting.
    // SAFETY: writing one byte from a valid buffer.
    unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
}

fn drain_pipe(fd: RawFd) {
    let mut buf = [0u8; 64];
    // SAFETY: reading into a valid buffer; the pipe is nonblocking.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

enum Io {
    Done(usize),
    /// Nothing to do right now.
    Again,
    /// The other side is gone (EOF / EIO / a hangup).
    Closed,
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> Io {
    // SAFETY: reading into a valid buffer of the given length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    match n {
        0 => Io::Closed,
        n if n > 0 => Io::Done(n as usize),
        _ => match io::Error::last_os_error().kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Io::Again,
            _ => Io::Closed,
        },
    }
}

fn write_fd(fd: RawFd, buf: &[u8]) -> Io {
    // SAFETY: writing from a valid buffer of the given length.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n >= 0 {
        return Io::Done(n as usize);
    }
    match io::Error::last_os_error().kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Io::Again,
        _ => Io::Closed,
    }
}

fn poll_fd(fd: RawFd, events: i16) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

/// `poll`, retried on EINTR. Returns how many descriptors are ready (0 on timeout).
fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> usize {
    let ms = timeout.map_or(-1, |d| d.as_millis().min(i32::MAX as u128) as i32);
    loop {
        // SAFETY: `fds` is a valid array of pollfd of the given length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n >= 0 {
            return n as usize;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return 0;
        }
    }
}

const READY: i16 = libc::POLLIN | libc::POLLHUP | libc::POLLERR;

struct OutputIo {
    p: OwnedFd,
    r: OwnedFd,
    stop: OwnedFd,
    wake: OwnedFd,
    wake_input: OwnedFd,
    shared: Arc<Shared>,
}

/// Shell output → tracker → VTE.
fn output_worker(io: OutputIo) {
    let mut tracker = copad_keys::Tracker::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut replies = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut reading = true;
    // After the shell exits: how much more reading time is left, and how long until the
    // output counts as quiet. Both run down only while the shell's PTY is actually being
    // read, so a VTE that is not taking output never cuts the drain short.
    let mut drain: Option<(Duration, Duration)> = None;
    let (p, r) = (io.p.as_raw_fd(), io.r.as_raw_fd());
    loop {
        if drain.is_none() && io.shared.child_exited.load(Ordering::Acquire) {
            drain = Some((DRAIN_MAX, DRAIN_QUIET));
        }
        if let Some((budget, quiet)) = drain
            && (budget.is_zero() || quiet.is_zero())
        {
            reading = false;
        }
        if !reading && pending.is_empty() {
            break;
        }
        let want_read = reading && pending.len() < BUFFER_CAP;
        let mut fds = vec![
            poll_fd(io.stop.as_raw_fd(), libc::POLLIN),
            poll_fd(io.wake.as_raw_fd(), libc::POLLIN),
        ];
        let p_at = want_read.then(|| {
            fds.push(poll_fd(p, libc::POLLIN));
            fds.len() - 1
        });
        let r_at = (!pending.is_empty()).then(|| {
            fds.push(poll_fd(r, libc::POLLOUT));
            fds.len() - 1
        });
        let timeout = drain
            .filter(|_| want_read)
            .map(|(budget, quiet)| budget.min(quiet));
        let started = Instant::now();
        poll(&mut fds, timeout);
        if want_read && let Some((budget, quiet)) = drain.as_mut() {
            let spent = started.elapsed();
            *budget = budget.saturating_sub(spent);
            *quiet = quiet.saturating_sub(spent);
        }
        if fds[0].revents != 0 {
            return;
        }
        if fds[1].revents != 0 {
            drain_pipe(io.wake.as_raw_fd());
        }
        if let Some(i) = p_at
            && fds[i].revents & READY != 0
        {
            match read_fd(p, &mut buf) {
                Io::Done(n) => {
                    tracker.feed(&buf[..n], &mut pending, &mut replies);
                    io.shared.flags.store(tracker.flags(), Ordering::Release);
                    io.shared
                        .app_cursor
                        .store(tracker.app_cursor(), Ordering::Release);
                    if let Some((_, quiet)) = drain.as_mut() {
                        *quiet = DRAIN_QUIET;
                    }
                    if !replies.is_empty() {
                        let mut q = io.shared.replies.lock().unwrap();
                        if q.len() + replies.len() <= REPLY_CAP {
                            q.extend(replies.iter());
                        }
                        replies.clear();
                        drop(q);
                        wake(io.wake_input.as_raw_fd());
                    }
                }
                Io::Again => {}
                // Every process holding the shell's terminal closed it.
                Io::Closed => reading = false,
            }
        }
        if let Some(i) = r_at
            && fds[i].revents != 0
        {
            match write_fd(r, &pending) {
                Io::Done(n) => {
                    pending.drain(..n);
                }
                Io::Again => {}
                // VTE dropped its side: nobody to deliver to.
                Io::Closed => return,
            }
        }
    }
    // Everything was delivered: let the input worker go, and close our side of R. VTE reads
    // EOF once the input worker's copy closes too.
    io.shared.output_done.store(true, Ordering::Release);
    wake(io.wake_input.as_raw_fd());
}

/// VTE's writes and the tracker's answers → the shell.
fn input_worker(r: OwnedFd, p: OwnedFd, stop: OwnedFd, wake_r: OwnedFd, shared: Arc<Shared>) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let (r, p) = (r.as_raw_fd(), p.as_raw_fd());
    loop {
        let mut fds = vec![
            poll_fd(stop.as_raw_fd(), libc::POLLIN),
            poll_fd(wake_r.as_raw_fd(), libc::POLLIN),
        ];
        let r_at = (pending.len() < BUFFER_CAP).then(|| {
            fds.push(poll_fd(r, libc::POLLIN));
            fds.len() - 1
        });
        let p_at = (!pending.is_empty()).then(|| {
            fds.push(poll_fd(p, libc::POLLOUT));
            fds.len() - 1
        });
        poll(&mut fds, None);
        if fds[0].revents != 0 {
            return;
        }
        if fds[1].revents != 0 {
            drain_pipe(wake_r.as_raw_fd());
            // Answers share the input buffer's bound: past it they are dropped, like the
            // ones that never fit the reply queue.
            let mut replies = shared.replies.lock().unwrap();
            let room = BUFFER_CAP.saturating_sub(pending.len()).min(replies.len());
            pending.extend(replies.drain(..room));
            replies.clear();
            drop(replies);
            if shared.output_done.load(Ordering::Acquire) {
                return;
            }
        }
        if let Some(i) = r_at
            && fds[i].revents & READY != 0
        {
            match read_fd(r, &mut buf) {
                Io::Done(n) => pending.extend_from_slice(&buf[..n]),
                Io::Again => {}
                Io::Closed => return,
            }
        }
        if let Some(i) = p_at
            && fds[i].revents != 0
        {
            match write_fd(p, &pending) {
                Io::Done(n) => {
                    pending.drain(..n);
                }
                Io::Again => {}
                // The shell is gone; its input goes nowhere.
                Io::Closed => pending.clear(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    /// A PTY pair standing in for the shell's: the test plays the shell on its slave.
    fn shell_pty() -> (OwnedFd, OwnedFd) {
        let (m, s) = open_raw_pty().unwrap();
        (m, s)
    }

    fn read_until(fd: RawFd, want: &[u8], timeout: Duration) -> Vec<u8> {
        let mut got = Vec::new();
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline && !got.windows(want.len()).any(|w| w == want) {
            let mut fds = [poll_fd(fd, libc::POLLIN)];
            if poll(&mut fds, Some(Duration::from_millis(20))) > 0 {
                match read_fd(fd, &mut buf) {
                    Io::Done(n) => got.extend_from_slice(&buf[..n]),
                    Io::Closed => break,
                    Io::Again => {}
                }
            }
        }
        got
    }

    fn write_all(fd: RawFd, mut data: &[u8]) {
        while !data.is_empty() {
            match write_fd(fd, data) {
                Io::Done(n) => data = &data[n..],
                Io::Again => std::thread::sleep(Duration::from_millis(1)),
                Io::Closed => panic!("closed"),
            }
        }
    }

    #[test]
    fn negotiation_is_taken_out_answered_and_published() {
        let (p_master, shell) = shell_pty();
        // The shell's PTY comes from VTE in packet mode; the relay must read plain bytes.
        let one: libc::c_int = 1;
        unsafe { libc::ioctl(p_master.as_raw_fd(), libc::TIOCPKT, &one) };
        let (vte, r_slave) = open_raw_pty().unwrap();
        let relay = Relay::start(p_master.as_fd(), r_slave).unwrap();
        write_all(shell.as_raw_fd(), b"hi\x1b[>1u\x1b[?u\x1b[cthere");
        let seen = read_until(vte.as_raw_fd(), b"there", Duration::from_secs(2));
        assert_eq!(seen, b"hi\x1b[cthere");
        let answer = read_until(shell.as_raw_fd(), b"u", Duration::from_secs(2));
        assert_eq!(answer, b"\x1b[?1u");
        assert_eq!(relay.flags(), 1);
        // VTE's own writes reach the shell untouched.
        write_all(vte.as_raw_fd(), b"\x1b[13;2u");
        let typed = read_until(shell.as_raw_fd(), b"2u", Duration::from_secs(2));
        assert_eq!(typed, b"\x1b[13;2u");
    }

    #[test]
    fn size_is_copied_to_the_shell() {
        let (p_master, shell) = shell_pty();
        let (_vte, r_slave) = open_raw_pty().unwrap();
        let relay = Relay::start(p_master.as_fd(), r_slave).unwrap();
        relay.set_size(40, 120);
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        unsafe { libc::ioctl(shell.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
        assert_eq!((ws.ws_row, ws.ws_col), (40, 120));
    }

    /// Output larger than both PTY buffers, written just before the shell goes away, all
    /// reaches VTE before its side closes — even while VTE is not reading for longer than
    /// both drain clocks.
    #[test]
    fn exit_delivers_everything_before_closing() {
        let (p_master, shell) = shell_pty();
        let (vte, r_slave) = open_raw_pty().unwrap();
        let relay = Relay::start(p_master.as_fd(), r_slave).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let writer = {
            let shell = shell.try_clone().unwrap();
            let payload = payload.clone();
            std::thread::spawn(move || {
                write_all(shell.as_raw_fd(), &payload);
                drop(shell);
            })
        };
        // VTE stalls well past DRAIN_MAX before it reads anything.
        std::thread::sleep(Duration::from_millis(300));
        drop(shell);
        relay.child_exited();
        std::thread::sleep(DRAIN_MAX + Duration::from_millis(200));
        let mut got = Vec::new();
        let mut buf = [0u8; 65536];
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let mut fds = [poll_fd(vte.as_raw_fd(), libc::POLLIN)];
            poll(&mut fds, Some(Duration::from_millis(100)));
            match read_fd(vte.as_raw_fd(), &mut buf) {
                Io::Done(n) => got.extend_from_slice(&buf[..n]),
                Io::Closed => break,
                Io::Again => {}
            }
        }
        writer.join().unwrap();
        assert_eq!(got.len(), payload.len());
        assert!(got == payload);
    }

    #[test]
    fn dropping_the_relay_stops_both_workers_even_when_blocked() {
        let (p_master, shell) = shell_pty();
        let (vte, r_slave) = open_raw_pty().unwrap();
        let relay = Relay::start(p_master.as_fd(), r_slave).unwrap();
        // The test's own ends must not block while it fills the buffers.
        set_nonblocking(vte.as_raw_fd()).unwrap();
        set_nonblocking(shell.as_raw_fd()).unwrap();
        // Fill VTE's side (nobody reads it) and the shell's input (nobody reads it either).
        let big = vec![b'x'; 512 * 1024];
        let w1 = {
            let s = shell.try_clone().unwrap();
            let big = big.clone();
            std::thread::spawn(move || {
                for chunk in big.chunks(4096) {
                    if let Io::Closed = write_fd(s.as_raw_fd(), chunk) {
                        break;
                    }
                }
            })
        };
        for chunk in big.chunks(4096) {
            let _ = write_fd(vte.as_raw_fd(), chunk);
        }
        drop(relay);
        drop(p_master);
        // With both workers gone, every copy of R's slave is closed: VTE's side reads EOF.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf = [0u8; 65536];
        let mut closed = false;
        while Instant::now() < deadline {
            match read_fd(vte.as_raw_fd(), &mut buf) {
                Io::Closed => {
                    closed = true;
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        assert!(closed);
        drop(shell);
        let _ = w1.join();
    }
}
