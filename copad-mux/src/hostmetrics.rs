//! Host machine metrics for the status surfaces: CPU, memory, GPU, load.
//!
//! A platform only implements [`imp::sample`]; the reading's shape, the poller, formatting and
//! config live here and in `hostpoll`.
//!
//! Every field is an `Option`, and absent means we could not read it, never zero: a status bar
//! rendering `cpu 0%` after a failed probe makes a false claim about the machine.
//!
//! * macOS: CPU and memory come from mach in-process; GPU from `ioreg`, the one subprocess,
//!   kept off the render loop and polled more slowly.
//! * Linux: CPU and memory from `/proc`; GPU from sysfs (amdgpu/i915), else `nvidia-smi`, gated
//!   on the NVIDIA module existing and killed on a deadline so a wedged driver cannot freeze the
//!   readings that share its thread.
//! * Anything else: every field `None`.

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
    pub gpu: Option<f64>,
    pub load1: Option<f64>,
}

impl HostMetrics {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.mem_used.is_none() && self.gpu.is_none() && self.load1.is_none()
    }

    pub fn mem_pct(&self) -> Option<f64> {
        let (used, total) = (self.mem_used?, self.mem_total?);
        (total > 0).then(|| used as f64 * 100.0 / total as f64)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CpuTicks {
    pub busy: u64,
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

pub fn load1() -> Option<f64> {
    let mut avg = [0f64; 3];
    // SAFETY: `getloadavg` writes at most `nelem` doubles into the array we hand it.
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
    (n >= 1).then_some(avg[0])
}

pub fn sample(prev: &mut CpuTicks, want_gpu: bool) -> HostMetrics {
    imp::sample(prev, want_gpu)
}

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

    /// The host port, acquired ONCE: `mach_host_self()` returns a send right and every call
    /// adds a reference that must be balanced with `mach_port_deallocate`. Calling it per
    /// sample leaked one reference every two seconds. Holding one for the process's life needs
    /// no deallocation, and the poller is the only caller.
    ///
    /// `libc::mach_host_self` is deprecated in favour of the `mach2` crate; kept to avoid a
    /// new dependency.
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

    /// GPU busy via `ioreg`: the ONE subprocess in this module (no public in-process API), so
    /// the poller runs it at a slower cadence and never on the render loop (decision #88).
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

    /// How long `nvidia-smi` gets before it is killed; a wedged driver makes it hang.
    const NVIDIA_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

    /// AMD and Intel expose a busy percentage in sysfs; NVIDIA needs `nvidia-smi`. Sysfs goes
    /// first because it costs no fork.
    fn gpu() -> Option<f64> {
        sysfs_gpu().or_else(nvidia_gpu)
    }

    /// The BUSIEST card, not the first that answers: with integrated plus discrete graphics
    /// enumeration order is arbitrary. Connector entries (`card1-DP-1`) share their card's
    /// `device`, so a card can be seen several times; a max does not care.
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

    /// Gated on the NVIDIA kernel module being loaded (a `stat`, not a fork), so a host with no
    /// NVIDIA card does not spawn a process every GPU tick just to fail.
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::{CpuTicks, HostMetrics, load1};

    pub fn sample(_prev: &mut CpuTicks, _want_gpu: bool) -> HostMetrics {
        HostMetrics {
            load1: load1(),
            ..HostMetrics::default()
        }
    }
}

/// `"Device Utilization %"=11` out of an `ioreg` dump. Lives outside the macOS module so other
/// platforms' CI can test it.
pub fn parse_ioreg_utilization(text: &str) -> Option<f64> {
    let key = "\"Device Utilization %\"=";
    let i = text.find(key)? + key.len();
    let digits: String = text[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<f64>().ok().map(|v| v.clamp(0.0, 100.0))
}

/// The aggregate `cpu` line of `/proc/stat` to tick counters. Field order:
/// `user nice system idle iowait irq softirq steal guest guest_nice`; trailing fields are
/// optional across kernel versions and absent means zero.
///
/// * `iowait` is idle time, not work (counting it reports a disk-bound box as pegged).
/// * `irq` and `softirq` ARE work (excluding them undercounts an interrupt-bound interval).
/// * `guest` and `guest_nice` are already included in `user`/`nice` by the kernel (see
///   `kernel/sched/cputime.c`), so summing every field double-counts them.
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
    // twice (saturating: a truncated line may not be self-consistent).
    let user = user.saturating_sub(guest);
    let nice = nice.saturating_sub(guest_nice);
    let busy = user + nice + system + irq + softirq + steal + guest + guest_nice;
    Some(CpuTicks {
        busy,
        total: busy + idle + iowait,
    })
}

/// `nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits` -> the busiest card.
/// `[N/A]` (a card that does not report utilization) must yield nothing rather than 0.
pub fn parse_nvidia_smi(text: &str) -> Option<f64> {
    text.lines()
        .filter_map(|l| l.trim().parse::<f64>().ok())
        // `"inf"` and `"nan"` parse as f64 and `clamp` on NaN returns NaN, which would poison
        // the whole reading.
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, 100.0))
        .reduce(f64::max)
}

/// Run a command and return its stdout, killing it if it outstays `deadline`, so a wedged probe
/// cannot freeze the poller thread (which also carries CPU, memory and load).
///
/// Only for commands whose output fits the pipe buffer: nothing drains stdout until the child
/// exits. That is why macOS's `ioreg` probe, whose dump runs to tens of kilobytes, uses
/// `output()` instead.
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
                // Reap on the timeout path too: an un-`wait`ed child keeps its pid and pipe.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            // Output is a few bytes, so it fits the pipe buffer and cannot deadlock.
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

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

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How often the host is sampled; also the CPU averaging window.
pub const TICK: Duration = Duration::from_secs(2);

/// Sample the GPU every Nth tick: it is the only reading that costs a subprocess.
const GPU_EVERY: u32 = 5;

/// How long one read keeps the poller sampling. With no reader (the top bar, `comux host`) the
/// poller sleeps instead of forking `ioreg` every two seconds for nobody.
const WANTED_FOR: Duration = Duration::from_secs(30);

pub struct Poll {
    metrics: Mutex<HostMetrics>,
    wanted_until: Mutex<Option<Instant>>,
    wake: Condvar,
}

pub type Shared = Arc<Poll>;

pub fn idle() -> Shared {
    Arc::new(Poll {
        metrics: Mutex::new(HostMetrics::default()),
        wanted_until: Mutex::new(None),
        wake: Condvar::new(),
    })
}

/// The latest reading, and a request to keep sampling for a while. Poison-safe: a panicked
/// poller degrades to "no reading" instead of taking the render loop down.
pub fn read(shared: &Shared) -> HostMetrics {
    if let Ok(mut w) = shared.wanted_until.lock() {
        let was_idle = w.is_none_or(|t| t <= Instant::now());
        *w = Some(Instant::now() + WANTED_FOR);
        if was_idle {
            // The cache may be hours old. Drop it HERE so this read already says "no reading"
            // (and `comux host` waits for a fresh one) instead of passing it off as current.
            if let Ok(mut m) = shared.metrics.lock() {
                *m = HostMetrics::default();
            }
            shared.wake.notify_all();
        }
    }
    shared.metrics.lock().map(|g| *g).unwrap_or_default()
}

/// Block until someone wants readings. Returns whether it had to wait (the counters are then a
/// whole idle period old).
fn wait_wanted(shared: &Poll) -> bool {
    let Ok(mut w) = shared.wanted_until.lock() else {
        return false;
    };
    let mut waited = false;
    while w.is_none_or(|t| t <= Instant::now()) {
        waited = true;
        w = match shared.wake.wait(w) {
            Ok(g) => g,
            Err(_) => return waited,
        };
    }
    waited
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
            loop {
                if wait_wanted(&out) || n == 0 {
                    // Prime the CPU counters so the first reading after a wake is a real rate
                    // rather than an average over the idle period.
                    let _ = sample(&mut ticks, false);
                    n = 0;
                }
                std::thread::sleep(TICK);
                let want_gpu = n.is_multiple_of(GPU_EVERY);
                let m = sample(&mut ticks, want_gpu);
                if let Ok(mut g) = out.metrics.lock() {
                    // A skipped GPU tick must not ERASE the last GPU reading: absent means
                    // "could not read", not "did not look".
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
    fn a_read_after_idle_never_returns_the_old_reading() {
        let p = idle();
        *p.metrics.lock().unwrap() = HostMetrics {
            cpu: Some(55.0),
            ..HostMetrics::default()
        };
        assert_eq!(read(&p).cpu, None);
        p.metrics.lock().unwrap().cpu = Some(12.0);
        assert_eq!(read(&p).cpu, Some(12.0));
        *p.wanted_until.lock().unwrap() = Some(Instant::now());
        assert_eq!(read(&p).cpu, None);
    }

    /// The host port must be acquired ONCE however many samples are taken: every
    /// `mach_host_self()` call adds a send-right reference that is never balanced. Asserts on
    /// the REAL port refcount, since the leak is invisible at the Rust level.
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
        let a = CpuTicks {
            busy: 100,
            total: 1000,
        };
        let b = CpuTicks {
            busy: 50,
            total: 500,
        };
        assert_eq!(cpu_percent(a, b), None);
        assert_eq!(cpu_percent(a, a), None);
    }

    #[test]
    fn proc_stat_counts_interrupt_work_but_not_iowait() {
        let s = "cpu  100 20 30 1000 500 1 2 3\ncpu0 1 1 1 1\n";
        let t = parse_proc_stat(s).unwrap();
        // irq + softirq + steal are WORK (excluding them while leaving them in the denominator
        // undercounts).
        assert_eq!(t.busy, 156, "user+nice+system+irq+softirq+steal");
        // iowait is idle: counting it as busy reports a disk-bound box as pegged.
        assert_eq!(t.total, 1656, "busy + idle + iowait");
        assert!(t.busy < 650);
    }

    #[test]
    fn guest_ticks_are_not_counted_twice() {
        // The kernel already folds guest into user and guest_nice into nice; this line is 100%
        // guest and summing every field reported 50% busy.
        let s = "cpu  100 10 0 0 0 0 0 0 100 10\n";
        let t = parse_proc_stat(s).unwrap();
        assert_eq!(t.busy, 110, "guest is inside user; it must be counted once");
        assert_eq!(t.total, 110);
        assert_eq!(cpu_percent(CpuTicks::default(), t), Some(100.0));
    }

    #[test]
    fn a_kernel_without_the_trailing_fields_still_parses() {
        // `steal`/`guest`/`guest_nice` arrived over successive kernel versions; absent is 0.
        let t = parse_proc_stat("cpu  10 0 5 85\n").unwrap();
        assert_eq!((t.busy, t.total), (15, 100));
    }

    #[test]
    fn proc_stat_tolerates_a_short_or_missing_line() {
        assert_eq!(parse_proc_stat("cpu  1 2 3\n"), None);
        assert_eq!(parse_proc_stat("intr 1 2 3 4\n"), None);
        assert_eq!(parse_proc_stat(""), None);
        assert_eq!(parse_proc_stat("cpu0 1 2 3 4\n"), None);
    }

    #[test]
    fn meminfo_uses_available_not_free() {
        let s = "MemTotal:       16384000 kB\nMemFree:          200000 kB\nMemAvailable:    8192000 kB\n";
        let (used, total) = parse_meminfo(s).unwrap();
        assert_eq!(total, 16_384_000 * 1024);
        assert_eq!(used, 8_192_000 * 1024, "used = total - MemAvailable");
        assert!(used * 100 / total < 60);
    }

    #[test]
    fn meminfo_falls_back_to_free_on_an_old_kernel() {
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
        assert_eq!(
            parse_ioreg_utilization(r#""Device Utilization %"=250"#),
            Some(100.0)
        );
        // The key present with no number must not parse as 0 (an idle GPU).
        assert_eq!(parse_ioreg_utilization(r#""Device Utilization %"=x"#), None);
    }

    #[test]
    fn nvidia_smi_reports_the_busiest_card() {
        assert_eq!(parse_nvidia_smi("24\n"), Some(24.0));
        // One line per GPU: the busiest wins, not whichever the driver enumerates first.
        assert_eq!(parse_nvidia_smi("3\n91\n17\n"), Some(91.0));
        assert_eq!(
            parse_nvidia_smi(" 42 \n"),
            Some(42.0),
            "whitespace is trimmed"
        );
    }

    #[test]
    fn an_unsupported_card_is_absent_not_idle() {
        // `[N/A]` must not parse as 0, which would claim an idle GPU.
        assert_eq!(parse_nvidia_smi("[N/A]\n"), None);
        assert_eq!(parse_nvidia_smi(""), None);
        assert_eq!(parse_nvidia_smi("Failed to initialize NVML\n"), None);
        // The N/A must not poison the reading, and neither must `inf`/`nan` (which parse as f64).
        assert_eq!(parse_nvidia_smi("[N/A]\n55\n"), Some(55.0));
        assert_eq!(parse_nvidia_smi("nan\ninf\n7\n"), Some(7.0));
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

        // This thread also carries CPU, memory and load, so a hanging probe would freeze every
        // reading forever.
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
        // A host that ADVERTISES a readable GPU must yield one. Conditioned on the host so the
        // test is meaningful on a machine with no GPU probe.
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
