//! Shared `setpgid` + `killpg` helpers every provider adapter uses
//! at spawn and teardown time.
//!
//! # Why this exists
//!
//! Tokio's `Command::kill_on_drop(true)` and explicit `start_kill()`
//! only terminate the *direct* child. If that child (a Node bridge,
//! a CLI binary, `opencode serve`) has forked its own children —
//! MCP stdio proxies, per-session workers, subshells for tool
//! invocations — those grandchildren do NOT receive the signal.
//! On Unix they reparent to PID 1 and survive the flowstate
//! process; on `tauri dev` hot-reload cycles they accumulate as
//! orphans pointing at a dead loopback port.
//!
//! The fix is to put every adapter-spawned child in its own
//! process group (via `setpgid(0, 0)` in a `pre_exec` hook) and
//! send `SIGTERM` to the whole group at drop time. That reaps the
//! entire subtree atomically in one `killpg` syscall.
//!
//! # Usage pattern
//!
//! ```no_run
//! use tokio::process::Command;
//! use zenui_provider_api::{enter_own_process_group, kill_process_group_best_effort};
//!
//! let mut cmd = Command::new("some-binary");
//! enter_own_process_group(&mut cmd);
//! let mut child = cmd.spawn().expect("spawn");
//! // Capture the pgid *after* spawn — on Unix it equals the child's pid
//! // because `setpgid(0, 0)` made the child its own group leader.
//! let pgid: Option<i32> = child.id().and_then(|p| i32::try_from(p).ok());
//! // ...hold `child` and `pgid` in your adapter's state struct...
//!
//! // When teardown runs (via Drop or an explicit kill path):
//! if let Some(pgid) = pgid {
//!     kill_process_group_best_effort(pgid);
//! }
//! let _ = child.start_kill();
//! ```
//!
//! # Windows
//!
//! Windows has no equivalent of POSIX process groups. Both helpers
//! are no-ops on non-Unix targets so the same code compiles
//! cross-platform; tokio's `kill_on_drop(true)` is still the sole
//! teardown mechanism there. If flowstate ever adds a Windows
//! release build that's sensitive to grandchild leaks, revisit via
//! Job Objects (`AssignProcessToJobObject`).

/// Configure `cmd` so the spawned child enters its own process
/// group (`setpgid(0, 0)` in a `pre_exec` hook). Call before
/// `.spawn()`. Unix-only; no-op on non-Unix targets.
#[cfg(unix)]
pub fn enter_own_process_group(cmd: &mut tokio::process::Command) {
    // Safety: `setpgid(0, 0)` is listed as async-signal-safe in
    // POSIX.1-2017 and is the canonical use case for tokio's
    // `pre_exec` hook. The closure runs in the forked child after
    // `fork()` returns but before `execve` — exactly the window
    // we need to make the child its own group leader.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

/// No-op on non-Unix platforms.
#[cfg(not(unix))]
pub fn enter_own_process_group(_cmd: &mut tokio::process::Command) {}

/// Send `SIGTERM` to every process in group `pgid`. Best-effort —
/// ignores `ESRCH` (the group is already empty, which is the
/// expected race on clean shutdown) and any other error.
///
/// Callers invoke this in `Drop` impls and in idle-watchdog kill
/// callbacks *before* tokio's `start_kill()` on the direct child,
/// so grandchildren get the TERM signal while the parent is still
/// alive to propagate it. Unix-only; no-op on non-Unix targets.
#[cfg(unix)]
pub fn kill_process_group_best_effort(pgid: i32) {
    // A pgid of 0 or negative is never something the spawn helper
    // above would produce, but guard anyway so a caller that
    // stored `Option<i32>::None` as `0` can't accidentally
    // broadcast SIGTERM to its own process group (which would
    // kill flowstate itself — `killpg(0, sig)` signals the
    // caller's group per POSIX).
    if pgid <= 0 {
        return;
    }
    // Safety: `killpg` is a standard POSIX syscall; passing a
    // valid positive pgid is the documented contract. No unwinding
    // across the FFI boundary.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
}

/// No-op on non-Unix platforms.
#[cfg(not(unix))]
pub fn kill_process_group_best_effort(_pgid: i32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn kill_rejects_non_positive_pgids() {
        // These would otherwise broadcast SIGTERM to the test
        // runner's own process group, which is exactly the
        // accident the pgid > 0 guard prevents. The assertion is
        // "we didn't suicide" — if the guard regresses, the test
        // binary dies before reporting.
        kill_process_group_best_effort(0);
        kill_process_group_best_effort(-1);
    }

    #[tokio::test]
    async fn setpgid_runs_before_exec() {
        // Spawn `sleep` in its own group and assert the child's
        // pgid equals its pid. Skipping on platforms where `sleep`
        // isn't at a standard path is cheaper than probing PATH.
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg("5").kill_on_drop(true);
        enter_own_process_group(&mut cmd);
        let child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("pid") as i32;
        // `getpgid(pid)` returns the pgid of `pid`. With our
        // pre_exec hook the child should be its own group leader.
        let pgid = unsafe { libc::getpgid(pid) };
        assert_eq!(pgid, pid, "child should lead its own group");
        drop(child);
    }
}
