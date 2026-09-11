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
//! * **Linux** — implemented from `/proc` and `sysfs` (plain file reads), but **not yet run on
//!   Linux**: this workspace's only host is macOS. Treat it as untested until it is.
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
    // `libc::mach_host_self` is deprecated in favour of the `mach2` crate. Kept rather than
    // taking a new dependency for two call sites: if libc ever removes it the build fails
    // loudly, which is the right failure for a migration that is a one-line swap.
    #[allow(deprecated)]
    fn cpu(prev: &mut CpuTicks) -> Option<f64> {
        let now = unsafe {
            let mut info: libc::host_cpu_load_info = zeroed();
            let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
            let r = libc::host_statistics(
                libc::mach_host_self(),
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
    #[allow(deprecated)]
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
                libc::mach_host_self(),
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

    /// AMD and Intel expose a busy percentage in sysfs. NVIDIA does not — it needs
    /// `nvidia-smi`, a subprocess, which is deliberately NOT added here until someone can
    /// run it: an untested fork in a long-lived server is worse than an absent reading.
    fn gpu() -> Option<f64> {
        let dir = std::fs::read_dir("/sys/class/drm").ok()?;
        for e in dir.flatten() {
            let p = e.path().join("device/gpu_busy_percent");
            if let Ok(t) = std::fs::read_to_string(&p)
                && let Ok(v) = t.trim().parse::<f64>()
            {
                return Some(v.clamp(0.0, 100.0));
            }
        }
        None
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
/// Fields after the first four are optional across kernel versions, so everything present is
/// summed into `total` and only user/nice/system count as `busy`. `iowait` is idle time, not
/// work — counting it as busy is the classic way to report a disk-bound box as CPU-pegged.
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
    let busy = vals[0] + vals[1] + vals[2];
    Some(CpuTicks {
        busy,
        total: vals.iter().sum(),
    })
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
    fn proc_stat_counts_iowait_as_idle_not_as_work() {
        // user nice system idle iowait irq softirq steal
        let s = "cpu  100 20 30 1000 500 1 2 3\ncpu0 1 1 1 1\n";
        let t = parse_proc_stat(s).unwrap();
        assert_eq!(t.busy, 150, "busy is user+nice+system only");
        assert_eq!(t.total, 1656, "every present field counts toward total");
        // Counting iowait as busy is the classic way to report a disk-bound box as pegged.
        assert!(t.busy < 650);
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
    }
}
