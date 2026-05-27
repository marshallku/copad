//! Cross-platform atomic-create-or-fail primitive.
//!
//! Used by KB-shape plugins (`copad-plugin-kb`, `copad-plugin-todo`,
//! `copad-plugin-bookmark`) that store one logical record per file and need
//! exactly-one-creator semantics: the loser of a race must observe
//! `AlreadyExists` rather than silently overwriting the winner's bytes.
//!
//! POSIX `rename(2)` alone replaces atomically — there is no portable flag
//! for "fail if destination exists." Each Unix flavor exposes its own
//! syscall:
//! - Linux: `renameat2(AT_FDCWD, from, AT_FDCWD, to, RENAME_NOREPLACE)`
//! - macOS: `renamex_np(from, to, RENAME_EXCL)` (also on iOS, watchOS)
//!
//! Other Unixes (FreeBSD, OpenBSD, NetBSD, …) would need their own
//! backend; we hard-fail at compile time rather than fall back to a
//! racy `stat` + `rename` because the whole point of this primitive is
//! the kernel-level guarantee.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `EEXIST` (`AlreadyExists`) when `to` is taken — `from` left for the
/// caller to clean up. Other syscall errors pass through. Bypasses
/// `std::fs::rename` because that maps to plain `rename(2)` which
/// silently replaces on Unix.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let from_c = path_to_cstring(from)?;
    let to_c = path_to_cstring(to)?;
    let r = unsafe { rename_no_replace_raw(from_c.as_ptr(), to_c.as_ptr()) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn path_to_cstring(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains nul byte: {}", p.display()),
        )
    })
}

#[cfg(target_os = "linux")]
unsafe fn rename_no_replace_raw(from: *const libc::c_char, to: *const libc::c_char) -> libc::c_int {
    // SAFETY: caller guarantees from/to point to valid NUL-terminated paths.
    unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from,
            libc::AT_FDCWD,
            to,
            libc::RENAME_NOREPLACE,
        )
    }
}

#[cfg(target_os = "macos")]
unsafe fn rename_no_replace_raw(from: *const libc::c_char, to: *const libc::c_char) -> libc::c_int {
    // `renamex_np` is the Darwin equivalent — `RENAME_EXCL` matches the
    // semantics of Linux's `RENAME_NOREPLACE` (errno EEXIST when target
    // already exists, leaving the source intact).
    // SAFETY: caller guarantees from/to point to valid NUL-terminated paths.
    unsafe { libc::renamex_np(from, to, libc::RENAME_EXCL) }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "copad_core::fs_atomic only supports Linux (renameat2) and macOS (renamex_np). \
     Add a platform branch in fs_atomic.rs for other Unixes."
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn creates_new_file() {
        let dir = tempdir();
        let from = dir.join("src");
        let to = dir.join("dst");
        fs::write(&from, b"hello").unwrap();
        rename_no_replace(&from, &to).expect("rename should succeed");
        assert_eq!(fs::read(&to).unwrap(), b"hello");
        assert!(!from.exists(), "source should be moved");
    }

    #[test]
    fn refuses_to_overwrite() {
        let dir = tempdir();
        let from = dir.join("src");
        let to = dir.join("dst");
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"existing").unwrap();
        let err = rename_no_replace(&from, &to).expect_err("rename should refuse overwrite");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&to).unwrap(), b"existing", "dst untouched");
        assert_eq!(fs::read(&from).unwrap(), b"new", "src untouched on EEXIST");
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "copad-fs-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }
}
