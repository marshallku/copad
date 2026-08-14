//! File-descriptor budget for the persistent server.
//!
//! Every pane costs about **five** descriptors: the PTY master, a `dup` of it kept
//! for foreground-pgrp queries, the PTY event loop's kqueue/epoll, and its wakeup
//! socketpair. A long-lived server hosting dozens of panes therefore needs a soft
//! `RLIMIT_NOFILE` in the thousands — but macOS still ships a **256** soft limit
//! (`launchctl limit maxfiles`), and the server does not open its own descriptors
//! through anything that would raise it. Because a detached server inherits the
//! limit of whatever spawned it (a login shell raises it, launchd and a GUI app do
//! not), a server born outside a terminal wedges at ~48 panes: from then on every
//! `new-tab` / `new-session` / split fails to spawn its shell with `EMFILE`, which
//! used to surface only as "the key did nothing".
//!
//! [`raise`] lifts the soft limit toward the hard limit at server boot, and
//! [`snapshot`] reports the resulting budget so `comux health` can show the
//! headroom instead of leaving the diagnosis to `lsof`.

use std::io;

/// Descriptors we try to secure for the server. At ~5 per pane this is room for
/// roughly 3000 panes — far past any real layout, while staying well under the
/// per-process ceilings (`kern.maxfilesperproc`) that would make `setrlimit` fail.
const DESIRED_NOFILE: libc::rlim_t = 16_384;

/// Fallbacks tried in order when [`DESIRED_NOFILE`] is refused. macOS rejects a soft
/// limit above `kern.maxfilesperproc` outright (and historically clamped
/// `RLIMIT_NOFILE` at `OPEN_MAX` = 10240), so a single attempt is not enough — an
/// older or more tightly configured kernel must still get *some* raise rather than
/// silently keeping 256.
const FALLBACK_NOFILE: [libc::rlim_t; 3] = [10_240, 4_096, 1_024];

/// Descriptors per pane (PTY master + its `dup`, the event loop's kqueue, and its
/// wakeup socketpair). Used to turn a raw limit into a pane budget in `health`.
pub const FDS_PER_PANE: usize = 5;

/// The server's descriptor budget at a point in time.
#[derive(Debug, Clone, Copy)]
pub struct FdBudget {
    /// Current soft `RLIMIT_NOFILE` — the ceiling that actually bites.
    pub soft: u64,
    /// Hard `RLIMIT_NOFILE`; `u64::MAX` stands in for `RLIM_INFINITY`.
    pub hard: u64,
    /// Descriptors currently open, or `None` if they could not be counted.
    pub open: Option<usize>,
}

impl FdBudget {
    /// How many more panes fit before the soft limit bites, given [`FDS_PER_PANE`].
    /// `None` when the open count is unavailable (nothing to subtract from).
    pub fn panes_remaining(&self) -> Option<usize> {
        let open = self.open?;
        Some((self.soft.saturating_sub(open as u64) as usize) / FDS_PER_PANE)
    }
}

/// Read the current soft/hard `RLIMIT_NOFILE`.
fn get() -> io::Result<(libc::rlim_t, libc::rlim_t)> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid, fully initialized `rlimit` we own.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((lim.rlim_cur, lim.rlim_max))
}

/// Try to set the soft limit to `soft`, keeping the hard limit as-is.
fn set(soft: libc::rlim_t, hard: libc::rlim_t) -> io::Result<()> {
    let lim = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    // SAFETY: `lim` is a valid, fully initialized `rlimit` we own.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Raise the soft `RLIMIT_NOFILE` toward the hard limit, returning `(before, after)`.
///
/// Never lowers an already-generous limit, and never asks for `RLIM_INFINITY` — the
/// kernel refuses it for `RLIMIT_NOFILE` on macOS, and an "unlimited" soft limit is
/// meaningless anyway when `kern.maxfilesperproc` is the real ceiling. Best-effort:
/// a kernel that refuses every candidate leaves the inherited limit in place rather
/// than failing server startup, since a low limit still runs — just with fewer panes.
///
/// Must be called before any threads are spawned only insofar as callers want the
/// raise to cover them; `setrlimit` itself is process-wide and thread-safe.
pub fn raise() -> io::Result<(u64, u64)> {
    let (before, hard) = get()?;
    // `RLIM_INFINITY` as the hard limit means "ask for whatever you want" — clamp our
    // request to DESIRED rather than propagating an infinite value into `rlim_cur`.
    let ceiling = if hard == libc::RLIM_INFINITY {
        DESIRED_NOFILE
    } else {
        DESIRED_NOFILE.min(hard)
    };
    let mut candidates = vec![ceiling];
    candidates.extend(FALLBACK_NOFILE.iter().copied().filter(|c| {
        // Only worth trying a fallback that is both below the failed request and
        // still an improvement on what we already have.
        *c < ceiling && *c > before
    }));
    for want in candidates {
        if want <= before {
            break; // already at least this generous — leave it alone
        }
        if set(want, hard).is_ok() {
            return Ok((before, want));
        }
    }
    Ok((before, before))
}

/// Count the process's open descriptors by probing each slot below the soft limit.
///
/// Portable across Linux and macOS (no `/proc`, no `libproc`) at the cost of one cheap
/// `fcntl` per slot, which is why the scan is capped: a shell that handed us a
/// million-descriptor limit must not turn this into a million syscalls.
///
/// A count is returned **only when the scan covered the whole range** the soft limit
/// allows. Past the cap there is no sound answer to give: descriptors are allocated
/// lowest-free-first, so a process only reaches beyond the cap by holding every slot
/// below it — but having done so once it can close low descriptors and keep high ones,
/// and no amount of scanning the low range distinguishes that from an honest count. So
/// a limit above the cap reports `None` rather than a number that might be wrong, and
/// callers fall back to the `EMFILE` errno check for their diagnosis.
///
/// In practice this only affects a server that INHERITED a huge limit: [`raise`] targets
/// [`DESIRED_NOFILE`], far below the cap, so the servers this crate starts are always
/// counted exactly — and a server with a million-descriptor limit has no headroom
/// question worth answering anyway.
fn count_open(soft: libc::rlim_t) -> Option<usize> {
    /// Enough to cover [`DESIRED_NOFILE`] many times over.
    const SCAN_CAP: libc::rlim_t = 65_536;
    if soft > SCAN_CAP {
        return None;
    }
    let scan = soft;
    let mut open = 0usize;
    for fd in 0..scan as i32 {
        // SAFETY: `F_GETFD` only reads the descriptor flags; an unused slot returns
        // -1/EBADF, which is exactly the signal we want.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1 {
            open += 1;
        }
    }
    Some(open)
}

/// The current descriptor budget: soft/hard limits plus how many are in use.
pub fn snapshot() -> io::Result<FdBudget> {
    let (soft, hard) = get()?;
    Ok(FdBudget {
        soft,
        hard: if hard == libc::RLIM_INFINITY {
            u64::MAX
        } else {
            hard
        },
        open: count_open(soft),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_sees_a_descriptor_we_just_opened() {
        // The test runner inherits the developer's shell limit, which may be far above
        // SCAN_CAP — and above the cap `count_open` deliberately declines to answer. Only
        // the countable case has something to assert here; `count_open_declines_above_the_cap`
        // covers the other side.
        let Some(before) = snapshot().expect("snapshot").open else {
            return;
        };
        let f = std::fs::File::open("/dev/null").expect("open /dev/null");
        let after = snapshot()
            .expect("snapshot")
            .open
            .expect("still countable — the limit did not change");
        assert!(
            after > before,
            "an extra open fd must be visible: {before} -> {after}"
        );
        drop(f);
    }

    #[test]
    fn count_open_declines_above_the_cap() {
        // A limit past the scan cap has no sound count: a descriptor above the cap with a
        // freed slot below it is indistinguishable from an honest lower count. Reporting
        // `None` is what keeps `panes_remaining` from ever overstating the headroom.
        assert_eq!(count_open(u64::from(u32::MAX)), None);
    }

    #[test]
    fn count_open_is_exact_at_the_limit_this_crate_sets() {
        // The limit `raise` actually installs must always be countable — that is the whole
        // basis for `health` reporting a headroom number.
        assert!(
            count_open(DESIRED_NOFILE).is_some(),
            "the raised limit must stay within the scannable range"
        );
    }

    #[test]
    fn raise_never_lowers_the_soft_limit() {
        let (before, _) = get().expect("getrlimit");
        let (reported_before, after) = raise().expect("raise");
        assert_eq!(reported_before, before, "reports the pre-raise limit");
        assert!(
            after >= before,
            "raise must never lower: {before} -> {after}"
        );
    }

    #[test]
    fn raise_reaches_a_usable_pane_budget() {
        raise().expect("raise");
        let budget = snapshot().expect("snapshot");
        // The whole point: enough headroom that a realistic layout cannot exhaust it.
        // 256 (the macOS default this fixes) would fail here.
        assert!(
            budget.soft >= 1_024,
            "soft limit should be raised to a usable value, got {}",
            budget.soft
        );
    }

    #[test]
    fn panes_remaining_divides_the_headroom() {
        let budget = FdBudget {
            soft: 256,
            hard: 256,
            open: Some(251),
        };
        // The exact budget the wedged server was sitting on: 5 free descriptors is
        // one pane's worth and nothing more.
        assert_eq!(budget.panes_remaining(), Some(1));
        let wedged = FdBudget {
            open: Some(256),
            ..budget
        };
        assert_eq!(wedged.panes_remaining(), Some(0));
    }

    #[test]
    fn panes_remaining_is_none_without_a_count() {
        let budget = FdBudget {
            soft: 16_384,
            hard: 16_384,
            open: None,
        };
        assert_eq!(budget.panes_remaining(), None);
    }
}
