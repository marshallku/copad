//! Platform-aware filesystem paths for copadd and its clients.

use std::env;
use std::path::PathBuf;

/// - Linux: `$XDG_RUNTIME_DIR/copad/` or `/tmp/copad-{uid}/` (uid-namespaced
///   so multi-user `/tmp` doesn't race on first-binder).
/// - macOS: `~/Library/Caches/copad/` (no XDG_RUNTIME_DIR equivalent).
pub fn runtime_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = env::var("XDG_RUNTIME_DIR")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("copad");
        }
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/copad-{uid}"))
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("Library/Caches/copad")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        PathBuf::from("/tmp/copad")
    }
}

/// `copadd` listens here; `coctl` connects here unless `COPAD_SOCKET`
/// overrides.
pub fn socket_path() -> PathBuf {
    if let Ok(override_path) = env::var("COPAD_SOCKET")
        && !override_path.is_empty()
    {
        return PathBuf::from(override_path);
    }
    runtime_dir().join("socket")
}

/// Daemon socket from a GUI's perspective. Same as [`socket_path`] but with
/// two extra guards:
///
/// 1. If `COPAD_SOCKET` was injected by a parent copad (to point a child
///    shell's coctl at the legacy per-instance socket
///    `/tmp/copad-{PID}.sock`), that's *not* the daemon — speaking the
///    daemon wire protocol to it produces `unknown_method` for every Request.
///    The legacy pattern is detected and falls through to the well-known path.
/// 2. The well-known fallback (`runtime_dir()/socket`) requires its parent
///    directory to pass [`is_trusted_dir`]. On systems without
///    `XDG_RUNTIME_DIR` that parent is `/tmp/copad-{uid}`, which an attacker
///    can pre-create. Returns `None` so the caller refuses to attach to an
///    untrusted daemon. An explicit `COPAD_SOCKET` override (non-legacy) is
///    treated as user-asserted trust — the user is responsible for that path.
pub fn daemon_socket_path() -> Option<PathBuf> {
    if let Ok(override_path) = env::var("COPAD_SOCKET")
        && !override_path.is_empty()
    {
        let p = PathBuf::from(override_path);
        if !is_legacy_per_instance_socket(&p) {
            return Some(p);
        }
        // Fall through to the well-known path.
    }
    let dir = runtime_dir();
    if is_trusted_dir(&dir) {
        Some(dir.join("socket"))
    } else {
        None
    }
}

/// True for `/tmp/copad-{PID}.sock` (older builds) or
/// `<runtime_dir>/gui-{PID}.sock` (current). Used by `daemon_socket_path`
/// to dodge a child shell that inherited a per-instance GUI socket from
/// its parent copad.
fn is_legacy_per_instance_socket(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("/tmp/copad-")
        && let Some(num) = rest.strip_suffix(".sock")
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    if let Some(parent) = p.parent()
        && parent == runtime_dir()
        && let Some(name) = p.file_name().and_then(|n| n.to_str())
        && let Some(rest) = name.strip_prefix("gui-")
        && let Some(num) = rest.strip_suffix(".sock")
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    false
}

/// GUI per-instance socket path. Lives under the trusted runtime dir so
/// fs-level permissions (parent 0700 + socket 0600) gate `connect(2)`.
/// One file per copad instance; PID-named for collision-free coexistence.
pub fn gui_socket_path(pid: u32) -> PathBuf {
    runtime_dir().join(format!("gui-{pid}.sock"))
}

/// Persistent state (handoffs, indices) — Linux `~/.local/state/copad/`,
/// macOS `~/Library/Application Support/copad/`.
pub fn state_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = env::var("XDG_STATE_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("copad");
        }
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local/state/copad")
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Library/Application Support/copad")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        PathBuf::from(".copad")
    }
}

/// Regenerable cache (wallpaper lists, derived indices).
pub fn cache_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = env::var("XDG_CACHE_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("copad");
        }
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cache/copad")
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Library/Caches/copad")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        PathBuf::from(".copad-cache")
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

/// Dir exists, is owned by current uid, and grants no group/other access.
/// Blocks the `/tmp/copad-{victim_uid}` pre-creation attack on systems
/// without `XDG_RUNTIME_DIR`.
pub fn is_trusted_dir(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    let current_uid = unsafe { libc::getuid() };
    if meta.uid() != current_uid {
        return false;
    }
    (meta.mode() & 0o077) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Poison-tolerant guard acquisition. A single panicking test (often
    /// a platform-specific behaviour mismatch like the macOS path of
    /// `daemon_socket_returns_none_for_untrusted_runtime_dir` below)
    /// would otherwise poison `ENV_LOCK` and cascade-fail every other
    /// env-touching test in the module. Poison just means a prior
    /// holder panicked; the actual env state is still serialized by
    /// the mutex, so taking `into_inner` is sound for this use.
    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// RAII guard: captures the pre-test value of an env var and restores
    /// it on `Drop`, so a panicking test (or one that ran with a prior
    /// value already set in the environment) doesn't pollute the rest of
    /// the test binary. Caller must hold [`ENV_LOCK`] for the duration of
    /// the guard's lifetime — the unsafe env mutation is only sound under
    /// that serialization.
    struct EnvVar {
        name: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvVar {
        fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prior = env::var_os(name);
            unsafe {
                env::set_var(name, value);
            }
            Self { name, prior }
        }

        fn unset(name: &'static str) -> Self {
            let prior = env::var_os(name);
            unsafe {
                env::remove_var(name);
            }
            Self { name, prior }
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            unsafe {
                match self.prior.take() {
                    Some(v) => env::set_var(self.name, v),
                    None => env::remove_var(self.name),
                }
            }
        }
    }

    #[test]
    fn socket_path_respects_env_override() {
        let _g = lock_env();
        let _sock = EnvVar::set("COPAD_SOCKET", "/custom/path/sock");
        assert_eq!(socket_path(), PathBuf::from("/custom/path/sock"));
    }

    #[test]
    fn runtime_dir_returns_nonempty() {
        // runtime_dir() reads XDG_RUNTIME_DIR, which other tests mutate
        // under ENV_LOCK — take the lock here too or we race against them.
        let _g = lock_env();
        let dir = runtime_dir();
        assert!(!dir.as_os_str().is_empty());
        assert!(dir.to_string_lossy().contains("copad"));
    }

    #[test]
    fn is_trusted_dir_rejects_missing() {
        let nonexistent = PathBuf::from("/tmp/copad-test-does-not-exist-123456");
        assert!(!is_trusted_dir(&nonexistent));
    }

    #[test]
    fn is_trusted_dir_rejects_world_accessible() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "copad-trust-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("loosen");
        assert!(!is_trusted_dir(&dir), "0755 dir must NOT be trusted");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("tighten");
        assert!(is_trusted_dir(&dir), "0700 dir owned by us IS trusted");
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn daemon_socket_ignores_legacy_per_instance_pattern() {
        let _g = lock_env();
        let _sock = EnvVar::set("COPAD_SOCKET", "/tmp/copad-3090.sock");
        let p = daemon_socket_path();
        assert_ne!(p, Some(PathBuf::from("/tmp/copad-3090.sock")));
    }

    #[test]
    fn daemon_socket_honors_genuine_override() {
        let _g = lock_env();
        let _sock = EnvVar::set("COPAD_SOCKET", "/tmp/my-custom-daemon.sock");
        let p = daemon_socket_path();
        assert_eq!(p, Some(PathBuf::from("/tmp/my-custom-daemon.sock")));
    }

    #[test]
    #[cfg(target_os = "linux")]
    // `runtime_dir()` only honors `XDG_RUNTIME_DIR` on Linux; macOS always
    // returns `~/Library/Caches/copad/`, which is a sandboxed user dir
    // (trusted by `is_trusted_dir`). Gating the test under target_os keeps
    // it meaningful where the override exists and stops the assert-failure
    // panic from poisoning `ENV_LOCK` for every other env-touching test on
    // macOS — see `lock_env`.
    fn daemon_socket_returns_none_for_untrusted_runtime_dir() {
        // 0755 XDG_RUNTIME_DIR exercises is_trusted_dir's rejection path
        // without needing root to chown a real /run/user dir.
        use std::os::unix::fs::PermissionsExt;
        let _g = lock_env();
        let dir = std::env::temp_dir().join(format!(
            "copad-untrusted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("loosen");
        let _sock = EnvVar::unset("COPAD_SOCKET");
        let _xdg = EnvVar::set("XDG_RUNTIME_DIR", &dir);
        let p = daemon_socket_path();
        let _ = std::fs::remove_dir(&dir);
        assert!(p.is_none(), "untrusted runtime dir must yield None");
    }

    #[test]
    fn gui_socket_path_is_pid_named_under_runtime_dir() {
        let _g = lock_env();
        let p = gui_socket_path(12345);
        assert!(p.starts_with(runtime_dir()));
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "gui-12345.sock");
    }

    #[test]
    fn daemon_socket_ignores_hardened_gui_pattern() {
        let _g = lock_env();
        let gui = gui_socket_path(99999);
        let _sock = EnvVar::set("COPAD_SOCKET", &gui);
        let p = daemon_socket_path();
        assert_ne!(p, Some(gui));
    }

    #[test]
    fn paths_are_distinct() {
        let _g = lock_env();
        let _sock = EnvVar::unset("COPAD_SOCKET");
        let sock = socket_path();
        let state = state_dir();
        let cache = cache_dir();
        assert_ne!(sock, state);
        assert_ne!(state, cache);
    }

    #[test]
    fn env_var_guard_restores_prior_value() {
        let _g = lock_env();
        let _seed = EnvVar::set("COPAD_TEST_SEED", "prior");
        {
            let _inner = EnvVar::set("COPAD_TEST_SEED", "temp");
            assert_eq!(env::var("COPAD_TEST_SEED").as_deref(), Ok("temp"));
        }
        assert_eq!(env::var("COPAD_TEST_SEED").as_deref(), Ok("prior"));
    }

    #[test]
    fn env_var_guard_restores_absence() {
        let _g = lock_env();
        let _seed = EnvVar::unset("COPAD_TEST_ABSENT");
        {
            let _inner = EnvVar::set("COPAD_TEST_ABSENT", "temp");
            assert!(env::var("COPAD_TEST_ABSENT").is_ok());
        }
        assert!(env::var("COPAD_TEST_ABSENT").is_err());
    }
}
