//! Unix signal helpers: SIGTERM/SIGKILL to a process group, and to
//! descendants that have left it.
//!
//! On non-Unix platforms these are no-ops; we only support macOS + Linux
//! today and the upstream `Command` lacks `process_group` elsewhere anyway.

#[cfg(unix)]
pub const SIGTERM: i32 = 15;
#[cfg(unix)]
pub const SIGKILL: i32 = 9;

/// How long `SIGKILL` waits for `SIGSTOP` to land before killing anyway.
///
/// The stop is what closes the fork window. This is a ceiling, not a delay:
/// once the leader and its descendants are stopped the kill proceeds.
#[cfg(unix)]
const SEAL_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

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

/// Signal `pgid` and every descendant still parented under that leader.
///
/// `SIGKILL` stops the leader before the fatal signal. A signal queued for
/// a process is not inherited by a child it forks before the signal is
/// delivered, so a bare `killpg` can miss a grandchild created in that
/// window — and it never sees a job the shell has moved into another group
/// (`set -m`). Stopping the leader first makes the fork visible, and the
/// parent walk reaches a child that already left the group, as long as it
/// has not been reparented to init.
#[cfg(unix)]
pub(crate) fn signal_tree(pgid: i32, sig: i32) {
    if !foreign(pgid) {
        return;
    }
    if sig == SIGKILL {
        seal_and_kill(pgid);
        return;
    }
    let _ = killpg(pgid, sig);
    for pid in descendants(pgid) {
        let _ = kill(pid, sig);
    }
}

#[cfg(not(unix))]
pub(crate) fn signal_tree(_pgid: i32, _sig: i32) {}

#[cfg(unix)]
fn seal_and_kill(leader: i32) {
    let _ = kill(leader, libc::SIGSTOP);
    let _ = killpg(leader, libc::SIGSTOP);

    let deadline = std::time::Instant::now() + SEAL_BUDGET;
    // Remember the stop. A later query can miss a one-shot status, and the
    // leader must not be killed while it can still fork.
    let mut leader_down = false;
    loop {
        if pid_quiescent(leader) {
            leader_down = true;
        }
        let mut pending = false;
        for pid in descendants(leader) {
            if pid_quiescent(pid) {
                continue;
            }
            let _ = kill(pid, libc::SIGSTOP);
            pending = true;
        }
        if leader_down && !pending {
            break;
        }
        if std::time::Instant::now() >= deadline {
            if !leader_down {
                tracing::warn!(
                    pgid = leader,
                    "kill-tree: leader still running after SIGSTOP; killing anyway"
                );
            }
            break;
        }
        std::thread::yield_now();
    }

    // Descendants first. Killing the leader first orphans a stopped child,
    // and the kernel then continues an orphaned process group.
    for pid in descendants(leader) {
        let _ = kill(pid, SIGKILL);
    }
    let _ = killpg(leader, SIGKILL);
    let _ = kill(leader, SIGKILL);
}

#[cfg(unix)]
fn kill(pid: i32, sig: i32) -> std::io::Result<()> {
    if !foreign(pid) {
        return Ok(());
    }
    // SAFETY: `kill` is a stable POSIX syscall. `pid` and `sig` are plain
    // integers; no memory is dereferenced. ESRCH means it is already gone.
    let rc = unsafe { libc::kill(pid, sig) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

/// `kill` of pid 0 or -1 is a process group, and our own pid is this daemon.
#[cfg(unix)]
fn foreign(pid: i32) -> bool {
    pid > 1 && std::process::id() != pid as u32
}

#[cfg(all(unix, target_os = "linux"))]
fn descendants(root: i32) -> Vec<i32> {
    let by_parent = linux_children();
    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    seen.insert(root);
    while let Some(pid) = stack.pop() {
        let Some(kids) = by_parent.get(&pid) else {
            continue;
        };
        for kid in kids {
            if foreign(*kid) && seen.insert(*kid) {
                out.push(*kid);
                stack.push(*kid);
            }
        }
    }
    out
}

#[cfg(all(unix, target_os = "linux"))]
fn linux_children() -> std::collections::HashMap<i32, Vec<i32>> {
    let mut by_parent: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return by_parent;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_state, ppid)) = state_and_ppid(&stat) else {
            continue;
        };
        by_parent.entry(ppid).or_default().push(pid);
    }
    by_parent
}

/// `pid (comm) state ppid ...`. `comm` may itself contain spaces and
/// parentheses, so the last `)` is the end of the name.
#[cfg(all(unix, target_os = "linux"))]
fn state_and_ppid(stat: &str) -> Option<(char, i32)> {
    let end = stat.rfind(')')?;
    let mut fields = stat[end + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

#[cfg(all(unix, target_os = "linux"))]
fn pid_quiescent(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    let Some((state, _)) = state_and_ppid(&stat) else {
        return true;
    };
    // T/t stopped, Z zombie, X dead. Anything else can still fork.
    matches!(state, 'T' | 't' | 'Z' | 'X' | 'x')
}

#[cfg(target_os = "macos")]
mod apple {
    /// `PROC_PIDTBSDINFO` from `<libproc.h>`.
    const PROC_PIDTBSDINFO: i32 = 3;
    /// `SSTOP` / `SZOMB` from `<sys/proc.h>`. `pbi_status` is the second `u32`.
    const SSTOP: u32 = 4;
    const SZOMB: u32 = 5;

    // `libproc` is not in `libc`. The signatures match current libproc.h:
    // `buffersize` is bytes, and `proc_listchildpids` returns a pid count
    // (the wrapper divides the `proc_listpids` byte count by `sizeof(int)`).
    #[link(name = "proc")]
    extern "C" {
        fn proc_listchildpids(ppid: i32, buffer: *mut libc::c_void, buffersize: i32) -> i32;
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: i32,
        ) -> i32;
    }

    pub(super) fn children(ppid: i32) -> Vec<i32> {
        // SAFETY: a null buffer and a zero size ask for the count. No memory
        // is written.
        let need = unsafe { proc_listchildpids(ppid, std::ptr::null_mut(), 0) };
        if need <= 0 {
            return Vec::new();
        }
        let cap = (need as usize + 16).min(4096);
        let mut buf = vec![0i32; cap];
        let bytes = match i32::try_from(buf.len() * std::mem::size_of::<i32>()) {
            Ok(bytes) => bytes,
            Err(_) => return Vec::new(),
        };
        // SAFETY: `buf` is writable for `bytes` bytes. The call writes `pid_t`s.
        let got = unsafe { proc_listchildpids(ppid, buf.as_mut_ptr().cast(), bytes) };
        if got <= 0 {
            return Vec::new();
        }
        let got = (got as usize).min(buf.len());
        buf.truncate(got);
        buf.into_iter().filter(|pid| super::foreign(*pid)).collect()
    }

    pub(super) fn quiescent(pid: i32) -> bool {
        // Larger than today's `proc_bsdinfo` so a longer struct still writes
        // `pbi_status` at offset 4.
        let mut buf = [0u8; 1024];
        // SAFETY: `buf` is writable. Flavor 3 is `PROC_PIDTBSDINFO`. A short
        // or failed read means the pid is gone or not ours to inspect.
        let n = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                buf.as_mut_ptr().cast(),
                buf.len() as i32,
            )
        };
        if n < 8 {
            return true;
        }
        let status = u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]);
        status == SSTOP || status == SZOMB
    }
}

#[cfg(target_os = "macos")]
fn descendants(root: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    seen.insert(root);
    while let Some(pid) = stack.pop() {
        for kid in apple::children(pid) {
            if seen.insert(kid) {
                out.push(kid);
                stack.push(kid);
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn pid_quiescent(pid: i32) -> bool {
    apple::quiescent(pid)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn descendants(_root: i32) -> Vec<i32> {
    Vec::new()
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn pid_quiescent(pid: i32) -> bool {
    // No non-reaping state query here. Existence is not "stopped", so the
    // seal waits out its budget and then kills the group anyway.
    kill(pid, 0).is_err()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::state_and_ppid;

    #[test]
    fn stat_line_keeps_the_ppid_when_comm_contains_parens() {
        assert_eq!(state_and_ppid("42 (my (cmd)) S 7 42 42"), Some(('S', 7)));
        assert_eq!(state_and_ppid("1 (systemd) T 0 1 1"), Some(('T', 0)));
        assert_eq!(state_and_ppid("nope"), None);
    }
}
