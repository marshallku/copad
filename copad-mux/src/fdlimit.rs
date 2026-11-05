//! File-descriptor budget for the persistent server.
//!
//! Every pane costs about five descriptors: the PTY master, a `dup` of it for foreground-pgrp
//! queries, the PTY event loop's kqueue/epoll, and its wakeup socketpair. macOS ships a 256 soft
//! `RLIMIT_NOFILE`, and a detached server inherits the limit of whatever spawned it (a login
//! shell raises it, launchd and a GUI app do not), so one born outside a terminal wedges at ~48
//! panes: every `new-tab` / `new-session` / split fails with `EMFILE`.
//!
//! [`raise`] lifts the soft limit toward the hard limit at server boot; [`snapshot`] reports the
//! resulting budget for `comux health`.

use std::io;

/// Descriptors we try to secure: room for roughly 3000 panes, well under per-process ceilings
/// like `kern.maxfilesperproc` that would make `setrlimit` fail.
const DESIRED_NOFILE: libc::rlim_t = 16_384;

/// Fallbacks tried in order when [`DESIRED_NOFILE`] is refused: macOS rejects a soft limit
/// above `kern.maxfilesperproc` outright, so a single attempt is not enough.
const FALLBACK_NOFILE: [libc::rlim_t; 3] = [10_240, 4_096, 1_024];

pub const FDS_PER_PANE: usize = 5;

#[derive(Debug, Clone, Copy)]
pub struct FdBudget {
    pub soft: u64,
    /// Hard `RLIMIT_NOFILE`; `u64::MAX` stands in for `RLIM_INFINITY`.
    pub hard: u64,
    pub open: Option<usize>,
}

impl FdBudget {
    pub fn panes_remaining(&self) -> Option<usize> {
        let open = self.open?;
        Some((self.soft.saturating_sub(open as u64) as usize) / FDS_PER_PANE)
    }
}

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
/// Never lowers an already-generous limit and never asks for `RLIM_INFINITY` (macOS refuses it).
/// Best-effort: if every candidate is refused the inherited limit stays rather than failing
/// startup.
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
    candidates.extend(
        FALLBACK_NOFILE
            .iter()
            .copied()
            .filter(|c| *c < ceiling && *c > before),
    );
    for want in candidates {
        if want <= before {
            break;
        }
        if set(want, hard).is_ok() {
            return Ok((before, want));
        }
    }
    Ok((before, before))
}

/// Count the process's open descriptors by probing each slot below the soft limit, with one
/// `fcntl` per slot, so the scan is capped.
///
/// Returns a count ONLY when the scan covered the whole range the soft limit allows: a process
/// can hold descriptors above the cap while having closed some below it, which a scan of the
/// low range cannot distinguish from an honest count. Callers fall back to the `EMFILE` errno
/// check. Servers this crate starts have a limit of [`DESIRED_NOFILE`], far below the cap.
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
        // The runner inherits the developer's shell limit, which may exceed SCAN_CAP, where
        // `count_open` declines to answer. Only the countable case is asserted here.
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
        // A limit past the scan cap has no sound count; `None` keeps `panes_remaining` from
        // ever overstating the headroom.
        assert_eq!(count_open(u64::from(u32::MAX)), None);
    }

    #[test]
    fn count_open_is_exact_at_the_limit_this_crate_sets() {
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
        // Enough headroom that a realistic layout cannot exhaust it; 256 (the macOS default)
        // would fail here.
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
        // 5 free descriptors is one pane's worth and nothing more.
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
