//! Host machine metrics for the status surfaces — CPU, memory, GPU, load.
//!
//! **The interface lives here; a platform only implements [`imp::sample`].** Everything that
//! could drift between platforms — the shape of a reading, what "absent" means, the poller,
//! the formatting, the config — is written once in this module and in `hostpoll`. A port adds
//! one `mod imp` and nothing else.
//!
//! Every field is an `Option`, and absent means **we could not read it**, never zero. A status
//! bar that renders `cpu 0%` when the probe failed is worse than one that renders nothing: 0%
//! is a claim about the machine.
//!
//! Platform status:
//! * **macOS** — implemented. CPU and memory come from mach in-process (`host_statistics` /
//!   `host_statistics64`), whose structs the `libc` crate already declares, so nothing is
//!   hand-transcribed. GPU comes from `ioreg`, the one subprocess, kept off the render loop
//!   and polled at a slower cadence than the rest.
//! * **Linux** — implemented and verified on hardware. CPU and memory are plain `/proc` reads;
//!   GPU comes from `sysfs` where amdgpu/i915 expose it, and otherwise from `nvidia-smi`, the
//!   one subprocess, gated on the NVIDIA module existing and killed on a deadline so a wedged
//!   driver cannot freeze the readings that share its thread.
//! * **Anything else** — every field `None`, which every surface already handles.

/// One reading of the host. All fields optional; see the module note on absence.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HostMetrics {
    /// Busy percentage across all cores, 0..100, averaged over the interval between the last
    /// two samples. `None` until a SECOND sample exists — a rate needs two points, and
    /// reporting the first sample as a value would report lifetime-average CPU as current.
    pub cpu: Option<f64>,
    /// Bytes of memory in use, by the definition macOS's Activity Monitor shows
    /// (active + wired + compressed) and Linux's `MemAvailable` complement.
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    /// GPU busy percentage, 0..100.
    pub gpu: Option<f64>,
    /// 1-minute load average.
    pub load1: Option<f64>,
}

impl HostMetrics {
    /// Nothing could be read at all — the surfaces render nothing rather than a row of dashes.
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.mem_used.is_none() && self.gpu.is_none() && self.load1.is_none()
    }

    /// Memory as a percentage, when both halves are known.
    pub fn mem_pct(&self) -> Option<f64> {
        let (used, total) = (self.mem_used?, self.mem_total?);
        (total > 0).then(|| used as f64 * 100.0 / total as f64)
    }
}

/// CPU tick counters carried between samples, so a platform can compute a rate. Opaque to
/// everything but [`imp`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CpuTicks {
    /// Ticks spent doing work (user + system + nice).
    pub busy: u64,
    /// Ticks in total (busy + idle + iowait + …).
    pub total: u64,
}

/// Busy percentage between two tick readings, or `None` when the counters did not advance
/// (the very first sample, a stalled clock, or a counter reset).
pub fn cpu_percent(prev: CpuTicks, now: CpuTicks) -> Option<f64> {
    let total = now.total.checked_sub(prev.total)?;
    let busy = now.busy.checked_sub(prev.busy)?;
    if total == 0 {
        return None;
    }
    Some((busy as f64 * 100.0 / total as f64).clamp(0.0, 100.0))
}

/// The 1-minute load average. POSIX, so it is shared rather than per-platform.
pub fn load1() -> Option<f64> {
    let mut avg = [0f64; 3];
    // SAFETY: `getloadavg` writes at most `nelem` doubles into the array we hand it.
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
    (n >= 1).then_some(avg[0])
}

/// Read the host. `prev` carries the CPU counters forward so a RATE can be computed; the
/// caller owns it (the poller thread).
pub fn sample(prev: &mut CpuTicks, want_gpu: bool) -> HostMetrics {
    imp::sample(prev, want_gpu)
}

// ===== macOS =====================================================================

#[cfg(target_os = "macos")]
mod imp {
    use super::{CpuTicks, HostMetrics, cpu_percent, load1};
    use std::mem::{size_of, zeroed};

    pub fn sample(prev: &mut CpuTicks, want_gpu: bool) -> HostMetrics {
        let (cpu, mem) = (cpu(prev), memory());
        HostMetrics {
            cpu,
            mem_used: mem.map(|m| m.0),
            mem_total: mem.map(|m| m.1),
            gpu: want_gpu.then(gpu).flatten(),
            load1: load1(),
        }
    }

    /// Aggregate CPU ticks via `host_statistics(HOST_CPU_LOAD_INFO)`. The payload is a flat
    /// `[natural_t; CPU_STATE_MAX]` that `libc` declares for us — there is no nested union to
    /// transcribe, which is what makes this safe to do by hand where other mach info calls
    /// are not.
    /// The host port, acquired ONCE.
    ///
    /// `mach_host_self()` returns a send RIGHT, and every call adds a reference that must be
    /// balanced with `mach_port_deallocate`. Calling it per sample leaked one reference every
    /// two seconds — measured growing 2 → 3 → 4 on successive calls — which on a server that
    /// runs for weeks is tens of thousands of references against a port that is never released.
    ///
    /// Holding one for the process's life is the standard fix and needs no deallocation: the
    /// port dies with the process, and the poller is the only caller.
    ///
    /// `libc::mach_host_self` is deprecated in favour of the `mach2` crate. Kept rather than
    /// taking a new dependency: if libc ever removes it the build fails loudly, which is the
    /// right failure for a migration that is a one-line swap.
    #[allow(deprecated)]
    pub(super) fn host_port() -> libc::mach_port_t {
        static PORT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    fn cpu(prev: &mut CpuTicks) -> Option<f64> {
        let now = unsafe {
            let mut info: libc::host_cpu_load_info = zeroed();
            let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
            let r = libc::host_statistics(
                host_port(),
                libc::HOST_CPU_LOAD_INFO,
                &mut info as *mut _ as *mut libc::integer_t,
                &mut count,
            );
            if r != 0 {
                return None;
            }
            let t = |i: i32| info.cpu_ticks[i as usize] as u64;
            CpuTicks {
                busy: t(libc::CPU_STATE_USER) + t(libc::CPU_STATE_SYSTEM) + t(libc::CPU_STATE_NICE),
                total: t(libc::CPU_STATE_USER)
                    + t(libc::CPU_STATE_SYSTEM)
                    + t(libc::CPU_STATE_NICE)
                    + t(libc::CPU_STATE_IDLE),
            }
        };
        let pct = cpu_percent(*prev, now);
        *prev = now;
        pct
    }

    /// `(used, total)` bytes. "Used" is active + wired + compressed, which is what Activity
    /// Monitor calls Memory Used. Deliberately NOT `top`'s figure, which folds in the file
    /// cache and so reads near-full on any machine that has been up a while.
    fn memory() -> Option<(u64, u64)> {
        unsafe {
            let mut total: u64 = 0;
            let mut len = size_of::<u64>();
            if libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                &mut total as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            ) != 0
            {
                return None;
            }
            let mut vm: libc::vm_statistics64 = zeroed();
            let mut count =
                (size_of::<libc::vm_statistics64>() / size_of::<libc::integer_t>()) as u32;
            if libc::host_statistics64(
                host_port(),
                libc::HOST_VM_INFO64,
                &mut vm as *mut _ as *mut libc::integer_t,
                &mut count,
            ) != 0
            {
                return None;
            }
            let page = libc::sysconf(libc::_SC_PAGESIZE) as u64;
            let used =
                (vm.active_count as u64 + vm.wire_count as u64 + vm.compressor_page_count as u64)
                    * page;
            Some((used, total))
        }
    }

    /// GPU busy via `ioreg`. The ONE subprocess in this module — there is no public in-process
    /// API for it — so the poller runs it at a slower cadence than the rest, and never on the
    /// render loop (decision #88).
    fn gpu() -> Option<f64> {
        for class in ["AGXAccelerator", "IOAccelerator"] {
            let out = std::process::Command::new("ioreg")
                .args(["-r", "-d", "1", "-c", class])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            if let Some(v) = super::parse_ioreg_utilization(&String::from_utf8_lossy(&out.stdout)) {
                return Some(v);
            }
        }
        None
    }
}

// ===== Linux =====================================================================

#[cfg(target_os = "linux")]
mod imp {
    use super::{CpuTicks, HostMetrics, cpu_percent, load1};

    pub fn sample(prev: &mut CpuTicks, want_gpu: bool) -> HostMetrics {
        let mem = memory();
        HostMetrics {
            cpu: cpu(prev),
            mem_used: mem.map(|m| m.0),
            mem_total: mem.map(|m| m.1),
            gpu: want_gpu.then(gpu).flatten(),
            load1: load1(),
        }
    }

    fn cpu(prev: &mut CpuTicks) -> Option<f64> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let now = super::parse_proc_stat(&stat)?;
        let pct = cpu_percent(*prev, now);
        *prev = now;
        pct
    }

    fn memory() -> Option<(u64, u64)> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        super::parse_meminfo(&text)
    }

    /// How long `nvidia-smi` gets before it is killed. It reads in ~25 ms here; a wedged
    /// driver makes it hang indefinitely, and this thread also carries CPU, memory and load.
    const NVIDIA_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

    /// AMD and Intel expose a busy percentage in sysfs; NVIDIA exposes it nowhere under
    /// `/proc` or `/sys` and needs `nvidia-smi`. Sysfs goes first because it costs no fork.
    fn gpu() -> Option<f64> {
        sysfs_gpu().or_else(nvidia_gpu)
    }

    /// The BUSIEST card, not the first one that answers: a machine with integrated plus
    /// discrete graphics lists both, and enumeration order is arbitrary — reading the first
    /// reports the idle iGPU while the dGPU is pinned. Connector entries (`card1-DP-1`) share
    /// their card's `device`, so a card can be seen several times; a max does not care.
    fn sysfs_gpu() -> Option<f64> {
        std::fs::read_dir("/sys/class/drm")
            .ok()?
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("device/gpu_busy_percent")).ok())
            .filter_map(|t| t.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 100.0))
            .reduce(f64::max)
    }

    /// Gated on the NVIDIA kernel module being loaded, which is a `stat` rather than a fork:
    /// a host with neither an amdgpu/i915 sysfs reading nor an NVIDIA card must not spawn a
    /// process every GPU tick forever just to fail.
    fn nvidia_gpu() -> Option<f64> {
        if !std::path::Path::new("/proc/driver/nvidia/gpus").exists() {
            return None;
        }
        let out = super::run_with_deadline(
            std::process::Command::new("nvidia-smi").args([
                "--query-gpu=utilization.gpu",
                "--format=csv,noheader,nounits",
            ]),
            NVIDIA_DEADLINE,
        )?;
        super::parse_nvidia_smi(&out)
    }
}

// ===== everything else ===========================================================

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::{CpuTicks, HostMetrics, load1};

    pub fn sample(_prev: &mut CpuTicks, _want_gpu: bool) -> HostMetrics {
        // `getloadavg` is POSIX, so even an unported platform reports something honest.
        HostMetrics {
            load1: load1(),
            ..HostMetrics::default()
        }
    }
}

// ===== parsers, shared so they are testable on any host ==========================

/// `"Device Utilization %"=11` out of an `ioreg` dump.
///
/// Lives outside the macOS module on purpose: a parser that can only be compiled on the
/// platform it parses for cannot be tested by anyone else's CI.
pub fn parse_ioreg_utilization(text: &str) -> Option<f64> {
    let key = "\"Device Utilization %\"=";
    let i = text.find(key)? + key.len();
    let digits: String = text[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<f64>().ok().map(|v| v.clamp(0.0, 100.0))
}

/// The aggregate `cpu` line of `/proc/stat` → tick counters.
///
/// Field order is fixed by the kernel:
/// `user nice system idle iowait irq softirq steal guest guest_nice`.
///
/// Two accounting rules that are easy to get wrong, and the first version got both:
/// * **`iowait` is idle time, not work.** Counting it as busy is the classic way to report a
///   disk-bound box as CPU-pegged.
/// * **`irq` and `softirq` ARE work.** Excluding them while leaving them in the denominator
///   undercounts: an interval spent entirely servicing interrupts reported ~0%.
/// * **`guest` and `guest_nice` are already included in `user`/`nice`** by the kernel, so
///   summing every field double-counts them. An interval spent wholly in a guest reported 50%.
///
/// Trailing fields are optional across kernel versions, so anything present is used and
/// anything absent is simply zero.
///
/// See `kernel/sched/cputime.c`.
pub fn parse_proc_stat(text: &str) -> Option<CpuTicks> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse::<u64>().ok())
        .collect();
    if vals.len() < 4 {
        return None;
    }
    let at = |i: usize| vals.get(i).copied().unwrap_or(0);
    let (user, nice, system, idle, iowait) = (at(0), at(1), at(2), at(3), at(4));
    let (irq, softirq, steal) = (at(5), at(6), at(7));
    let (guest, guest_nice) = (at(8), at(9));
    // `user`/`nice` already contain the guest ticks; subtract them so they are not counted
    // twice, saturating because a truncated or synthetic line may not be self-consistent.
    let user = user.saturating_sub(guest);
    let nice = nice.saturating_sub(guest_nice);
    let busy = user + nice + system + irq + softirq + steal + guest + guest_nice;
    Some(CpuTicks {
        busy,
        total: busy + idle + iowait,
    })
}

/// `nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits` → the busiest card.
///
/// One line per GPU, so this takes the max rather than the first line, for the same reason the
/// sysfs walk does. `[N/A]` — what nvidia-smi prints for a card that does not report
/// utilization — must yield nothing rather than 0: a 0 is a claim that the GPU is idle.
pub fn parse_nvidia_smi(text: &str) -> Option<f64> {
    text.lines()
        .filter_map(|l| l.trim().parse::<f64>().ok())
        // `"inf"` and `"nan"` parse as f64, and `clamp` on a NaN returns NaN rather than a
        // bound, so a junk line would otherwise poison the whole reading.
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, 100.0))
        .reduce(f64::max)
}

/// Run a command and return its stdout, killing it if it outstays `deadline`.
///
/// A probe must not be able to hang the poller: that one thread also carries CPU, memory and
/// load, so a single wedged subprocess would freeze every reading — permanently, since nothing
/// else ever times it out. `std::process` has no timed wait and this is the only call site, so
/// the loop is cheaper than a dependency.
///
/// **Only for commands whose output fits the pipe buffer.** Nothing drains stdout until the
/// child exits, so a chatty command would block writing and then be killed at the deadline.
/// That is why macOS's `ioreg` probe, whose dump runs to tens of kilobytes, keeps using
/// `output()` and is NOT routed through here.
#[cfg(target_os = "linux")]
fn run_with_deadline(cmd: &mut std::process::Command, deadline: Duration) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() >= deadline => {
                // Reaping matters on the timeout path too: an un-`wait`ed child keeps its pid
                // reserved and its end of the pipe open.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            // Output is a few bytes, so it sits in the pipe buffer and leaving it undrained
            // until the child exits cannot deadlock.
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

/// `/proc/meminfo` → `(used, total)` bytes, using `MemAvailable` (the kernel's own estimate of
/// what a new allocation could get) rather than `MemFree`, which excludes reclaimable cache
/// and so reports almost every Linux box as out of memory.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let kb = |key: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
    };
    let total = kb("MemTotal:")?;
    let avail = kb("MemAvailable:").or_else(|| kb("MemFree:"))?;
    Some((total.saturating_sub(avail) * 1024, total * 1024))
}

// ===== the poller ================================================================

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the host is sampled. Also the CPU averaging window, since the percentage is the
/// rate between consecutive samples.
const TICK: Duration = Duration::from_secs(2);

/// Sample the GPU every Nth tick. It is the only reading that costs a subprocess, and a GPU
/// busy figure does not need 2-second resolution in a status bar.
const GPU_EVERY: u32 = 5;

pub type Shared = Arc<Mutex<HostMetrics>>;

/// A handle that never updates — the client, and any path with no server.
pub fn idle() -> Shared {
    Arc::new(Mutex::new(HostMetrics::default()))
}

/// The latest reading. Poison-safe: a panicked poller degrades to "no reading", which every
/// surface already handles, rather than taking the render loop down with it.
pub fn read(shared: &Shared) -> HostMetrics {
    shared.lock().map(|g| *g).unwrap_or_default()
}

/// Start sampling on a dedicated thread. Never on the render loop: the GPU probe forks, and
/// even the in-process readings are syscalls we do not want between frames (decision #88).
pub fn spawn() -> Shared {
    let shared = idle();
    let out = shared.clone();
    let _ = std::thread::Builder::new()
        .name("host-poll".into())
        .spawn(move || {
            let mut ticks = CpuTicks::default();
            let mut n: u32 = 0;
            // Prime the CPU counters so the FIRST published reading is a real rate rather
            // than a machine-lifetime average.
            let _ = sample(&mut ticks, false);
            loop {
                std::thread::sleep(TICK);
                let want_gpu = n.is_multiple_of(GPU_EVERY);
                let m = sample(&mut ticks, want_gpu);
                if let Ok(mut g) = out.lock() {
                    // A skipped GPU tick must not ERASE the last GPU reading — absent means
                    // "could not read", and "we did not look this time" is not that.
                    let gpu = if want_gpu { m.gpu } else { g.gpu };
                    *g = HostMetrics { gpu, ..m };
                }
                n = n.wrapping_add(1);
            }
        });
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host port must be acquired ONCE however many samples are taken.
    ///
    /// `mach_host_self()` hands back a send right and every call adds a reference that is
    /// never balanced; the first version called it twice per sample, so a server running for
    /// weeks accumulated tens of thousands of references on a port it never released. Measured
    /// on this machine: three naive calls moved the count 2 → 5.
    ///
    /// Asserts on the REAL port refcount rather than on "we called a cached function", because
    /// the bug was invisible at the Rust level — the code looked perfectly ordinary.
    #[test]
    #[cfg(target_os = "macos")]
    fn sampling_does_not_leak_a_mach_port_reference() {
        unsafe extern "C" {
            fn mach_task_self() -> libc::mach_port_t;
            fn mach_port_get_refs(
                task: libc::mach_port_t,
                name: libc::mach_port_t,
                right: u32,
                refs: *mut u32,
            ) -> libc::kern_return_t;
        }
        const MACH_PORT_RIGHT_SEND: u32 = 0;
        let port = imp::host_port();
        let refs = || unsafe {
            let mut n: u32 = 0;
            mach_port_get_refs(mach_task_self(), port, MACH_PORT_RIGHT_SEND, &mut n);
            n
        };
        let before = refs();
        let mut ticks = CpuTicks::default();
        for _ in 0..20 {
            let _ = sample(&mut ticks, false);
        }
        assert_eq!(
            refs(),
            before,
            "20 samples moved the host port's send-right count — it is being re-acquired"
        );
    }

    #[test]
    fn a_cpu_percentage_needs_two_points() {
        // The first sample has nothing to diff against: reporting it would publish the
        // machine's LIFETIME average as its current load.
        assert_eq!(cpu_percent(CpuTicks::default(), CpuTicks::default()), None);
        let a = CpuTicks {
            busy: 100,
            total: 1000,
        };
        let b = CpuTicks {
            busy: 150,
            total: 1200,
        };
        assert_eq!(cpu_percent(a, b), Some(25.0));
    }

    #[test]
    fn counters_that_go_backwards_yield_nothing() {
        // A counter reset (or a reordered read) must not produce a huge or negative figure.
        let a = CpuTicks {
            busy: 100,
            total: 1000,
        };
        let b = CpuTicks {
            busy: 50,
            total: 500,
        };
        assert_eq!(cpu_percent(a, b), None);
        // Neither must a stalled clock divide by zero.
        assert_eq!(cpu_percent(a, a), None);
    }

    #[test]
    fn proc_stat_counts_interrupt_work_but_not_iowait() {
        // user nice system idle iowait irq softirq steal
        let s = "cpu  100 20 30 1000 500 1 2 3\ncpu0 1 1 1 1\n";
        let t = parse_proc_stat(s).unwrap();
        // irq + softirq + steal are WORK. The first version of this function excluded them
        // while leaving them in the denominator, and THIS TEST asserted 150 — it pinned the
        // bug rather than the behaviour, which is how it survived review-by-testing.
        assert_eq!(t.busy, 156, "user+nice+system+irq+softirq+steal");
        // iowait is idle: counting it as busy reports a disk-bound box as pegged.
        assert_eq!(t.total, 1656, "busy + idle + iowait");
        assert!(t.busy < 650);
    }

    #[test]
    fn guest_ticks_are_not_counted_twice() {
        // The kernel already folds guest into user and guest_nice into nice. Summing every
        // field double-counts them: this line is 100% guest, and the naive version called it
        // 50% busy.
        // user nice system idle iowait irq softirq steal guest guest_nice
        let s = "cpu  100 10 0 0 0 0 0 0 100 10\n";
        let t = parse_proc_stat(s).unwrap();
        assert_eq!(t.busy, 110, "guest is inside user; it must be counted once");
        assert_eq!(t.total, 110);
        assert_eq!(cpu_percent(CpuTicks::default(), t), Some(100.0));
    }

    #[test]
    fn a_kernel_without_the_trailing_fields_still_parses() {
        // `steal`/`guest`/`guest_nice` arrived over successive kernel versions; absent is 0,
        // not a refusal.
        let t = parse_proc_stat("cpu  10 0 5 85\n").unwrap();
        assert_eq!((t.busy, t.total), (15, 100));
    }

    #[test]
    fn proc_stat_tolerates_a_short_or_missing_line() {
        assert_eq!(parse_proc_stat("cpu  1 2 3\n"), None);
        assert_eq!(parse_proc_stat("intr 1 2 3 4\n"), None);
        assert_eq!(parse_proc_stat(""), None);
        // `cpu0` is a per-core line and must not be mistaken for the aggregate.
        assert_eq!(parse_proc_stat("cpu0 1 2 3 4\n"), None);
    }

    #[test]
    fn meminfo_uses_available_not_free() {
        let s = "MemTotal:       16384000 kB\nMemFree:          200000 kB\nMemAvailable:    8192000 kB\n";
        let (used, total) = parse_meminfo(s).unwrap();
        assert_eq!(total, 16_384_000 * 1024);
        assert_eq!(used, 8_192_000 * 1024, "used = total - MemAvailable");
        // `MemFree` alone would call this box 98.8% full while half its memory is reclaimable.
        assert!(used * 100 / total < 60);
    }

    #[test]
    fn meminfo_falls_back_to_free_on_an_old_kernel() {
        // MemAvailable arrived in Linux 3.14; before that MemFree is all there is.
        let s = "MemTotal:       1000 kB\nMemFree:         400 kB\n";
        assert_eq!(parse_meminfo(s), Some((600 * 1024, 1000 * 1024)));
        assert_eq!(
            parse_meminfo("MemFree: 400 kB\n"),
            None,
            "no total, no answer"
        );
    }

    #[test]
    fn ioreg_utilization_is_read_and_clamped() {
        let s = r#"  "PerformanceStatistics" = {"Tiler Utilization %"=6,"Device Utilization %"=11,"x"=0}"#;
        assert_eq!(parse_ioreg_utilization(s), Some(11.0));
        assert_eq!(parse_ioreg_utilization("nothing here"), None);
        // A value outside 0..100 is a bad reading, not a bad machine.
        assert_eq!(
            parse_ioreg_utilization(r#""Device Utilization %"=250"#),
            Some(100.0)
        );
        // The key present but with no number must not parse as 0 — that would claim an idle
        // GPU where we in fact read nothing.
        assert_eq!(parse_ioreg_utilization(r#""Device Utilization %"=x"#), None);
    }

    #[test]
    fn nvidia_smi_reports_the_busiest_card() {
        assert_eq!(parse_nvidia_smi("24\n"), Some(24.0));
        // One line per GPU. Reading the first would report whichever card the driver happens
        // to enumerate first, which is the same bug the sysfs walk had.
        assert_eq!(parse_nvidia_smi("3\n91\n17\n"), Some(91.0));
        assert_eq!(
            parse_nvidia_smi(" 42 \n"),
            Some(42.0),
            "whitespace is trimmed"
        );
    }

    #[test]
    fn an_unsupported_card_is_absent_not_idle() {
        // nvidia-smi prints `[N/A]` for a card that does not report utilization. Parsing that
        // as 0 would claim an idle GPU where in fact we read none.
        assert_eq!(parse_nvidia_smi("[N/A]\n"), None);
        assert_eq!(parse_nvidia_smi(""), None);
        assert_eq!(parse_nvidia_smi("Failed to initialize NVML\n"), None);
        // A readable card alongside an unreadable one still answers — the N/A must not poison
        // the reading, and `inf`/`nan` (which do parse as f64) must not either.
        assert_eq!(parse_nvidia_smi("[N/A]\n55\n"), Some(55.0));
        assert_eq!(parse_nvidia_smi("nan\ninf\n7\n"), Some(7.0));
        // Out of range is a bad reading, not a bad machine.
        assert_eq!(parse_nvidia_smi("250\n"), Some(100.0));
        assert_eq!(parse_nvidia_smi("-5\n"), Some(0.0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_wedged_probe_is_killed_rather_than_left_to_freeze_the_poller() {
        let mut ok = std::process::Command::new("echo");
        ok.arg("41");
        assert_eq!(
            run_with_deadline(&mut ok, Duration::from_secs(5)).as_deref(),
            Some("41\n")
        );

        // The reason the deadline exists: this thread also carries CPU, memory and load, so a
        // probe allowed to hang would freeze every reading forever.
        let start = std::time::Instant::now();
        let mut hang = std::process::Command::new("sleep");
        hang.arg("30");
        assert_eq!(
            run_with_deadline(&mut hang, Duration::from_millis(200)),
            None
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the deadline did not fire: {:?}",
            start.elapsed()
        );

        // A command that is not installed is an absent reading, not a panic.
        let mut missing = std::process::Command::new("copad-no-such-binary-exists");
        assert_eq!(
            run_with_deadline(&mut missing, Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn an_empty_reading_is_distinguishable_from_a_zero_one() {
        assert!(HostMetrics::default().is_empty());
        let zeroed = HostMetrics {
            cpu: Some(0.0),
            ..HostMetrics::default()
        };
        assert!(!zeroed.is_empty(), "0% CPU is a reading; no CPU is not");
        assert_eq!(HostMetrics::default().mem_pct(), None);
        let m = HostMetrics {
            mem_used: Some(1),
            mem_total: Some(0),
            ..HostMetrics::default()
        };
        assert_eq!(m.mem_pct(), None, "a zero total is not a 100% machine");
    }

    #[test]
    #[ignore = "reads this machine"]
    fn live_host_reading_is_plausible() {
        let mut t = CpuTicks::default();
        let _ = sample(&mut t, false);
        std::thread::sleep(Duration::from_millis(400));
        let m = sample(&mut t, true);
        println!("{m:?}  mem_pct={:?}", m.mem_pct());
        assert!(!m.is_empty(), "nothing at all was readable on this host");
        if let Some(c) = m.cpu {
            assert!((0.0..=100.0).contains(&c), "cpu out of range: {c}");
        }
        if let Some(p) = m.mem_pct() {
            assert!((0.0..=100.0).contains(&p), "mem out of range: {p}");
        }
        // A host that ADVERTISES a readable GPU must yield one. Conditioned on the host rather
        // than asserted outright, so the same test is meaningful on a machine with no GPU
        // probe at all instead of encoding whichever workstation last ran it.
        #[cfg(target_os = "linux")]
        if std::path::Path::new("/proc/driver/nvidia/gpus").exists()
            || std::fs::read_dir("/sys/class/drm")
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| e.path().join("device/gpu_busy_percent").exists())
        {
            let gpu = m
                .gpu
                .expect("this host advertises a GPU probe but none was read");
            assert!((0.0..=100.0).contains(&gpu), "gpu out of range: {gpu}");
        }
    }
}
