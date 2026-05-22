//! Unix signal helpers: SIGTERM/SIGKILL to a process group.
//!
//! On non-Unix platforms these are no-ops (returning `Ok(())`); we only
//! support macOS + Linux today and the upstream `Command` lacks
//! `process_group` elsewhere anyway.

#[cfg(unix)]
pub const SIGTERM: i32 = 15;
#[cfg(unix)]
pub const SIGKILL: i32 = 9;

/// Send `sig` to every process in the group identified by `pgid`.
///
/// Equivalent to `killpg(2)`. Treats "no such process" (ESRCH) as success:
/// the group already exited, which is exactly what we wanted.
#[cfg(unix)]
pub fn killpg(pgid: i32, sig: i32) -> std::io::Result<()> {
    // SAFETY: `killpg` is a stable POSIX syscall. `pgid` and `sig` are
    // plain integers; no memory is dereferenced. ESRCH is normalized to Ok.
    let rc = unsafe { libc::killpg(pgid, sig) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

#[cfg(not(unix))]
pub fn killpg(_pgid: i32, _sig: i32) -> std::io::Result<()> {
    Ok(())
}
