//! Cross-platform "kill the whole subtree at teardown" helper for
//! every provider adapter that spawns long-lived child processes.
//!
//! # Why this exists
//!
//! Tokio's `Command::kill_on_drop(true)` and explicit `start_kill()`
//! only terminate the *direct* child. If that child (a Node bridge,
//! a CLI binary, `opencode serve`) has forked its own children —
//! MCP stdio proxies, per-session workers, subshells for tool
//! invocations — those grandchildren do NOT receive the signal.
//! On Unix they reparent to PID 1 and survive flowstate; on
//! `tauri dev` hot-reload cycles they accumulate as orphans
//! pointing at a dead loopback port.
//!
//! # Cross-platform abstraction
//!
//! The fix is OS-specific:
//!
//! - **Unix**: put every adapter-spawned child in its own process
//!   group via `setpgid(0, 0)` in a `pre_exec` hook, and send
//!   `SIGTERM` to the whole group at drop time via `killpg`. Reaps
//!   the entire subtree atomically in one syscall.
//!
//! - **Windows**: create a Job Object with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, assign the spawned child
//!   to it via `AssignProcessToJobObject` after spawn, and either
//!   close the handle (auto-kills via the flag) or call
//!   `TerminateJobObject` for explicit teardown. Job Objects
//!   propagate to descendants automatically — even better than
//!   process groups, which only catch direct descendants of the
//!   group leader (and lose the membership on `setsid`).
//!
//! [`ProcessGroup`] presents a uniform API that hides these
//! differences: callers don't see `pgid`s or `HANDLE`s.
//!
//! # Usage
//!
//! ```no_run
//! use tokio::process::Command;
//! use zenui_provider_api::ProcessGroup;
//!
//! # async fn run() -> Result<(), std::io::Error> {
//! let mut cmd = Command::new("some-binary");
//! let mut group = ProcessGroup::before_spawn(&mut cmd);
//! let mut child = cmd.spawn()?;
//! group.attach(&child);
//!
//! // ...hold both `child` and `group` in your adapter's state struct...
//!
//! // When teardown runs (Drop or explicit kill path):
//! group.kill_best_effort();
//! let _ = child.start_kill();
//! # Ok(()) }
//! ```

use tokio::process::{Child, Command};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    },
};

/// Cross-platform process-group / Job-Object container. Owns the
/// platform-specific handle that lets [`Self::kill_best_effort`] reap
/// an entire subtree of descendants atomically.
///
/// On non-Unix non-Windows targets every method is a no-op and the
/// struct carries no state — `tokio::process::Command::kill_on_drop`
/// is the sole teardown mechanism on those platforms.
pub struct ProcessGroup {
    inner: ProcessGroupInner,
}

impl std::fmt::Debug for ProcessGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debug is opaque: callers that store ProcessGroup inside a
        // `#[derive(Debug)]` struct (e.g. ClaudeBridgeProcess) only
        // need the field to *exist* in the formatted output, not to
        // dump platform internals. The pgid would race with the
        // child's own lifetime; the Win32 HANDLE is just a pointer.
        f.debug_struct("ProcessGroup").finish_non_exhaustive()
    }
}

#[cfg(unix)]
struct ProcessGroupInner {
    /// pgid == child pid after `setpgid(0, 0)` runs in `pre_exec`. 0
    /// means the group hasn't been bound yet (e.g. [`ProcessGroup::attach`]
    /// hasn't been called) or the spawned child was already reaped
    /// before we could read its pid.
    pgid: i32,
}

#[cfg(windows)]
struct ProcessGroupInner {
    /// Job Object handle returned by `CreateJobObjectW`. NULL means
    /// creation failed (extremely rare — out of handles); we still
    /// store the struct so `Drop` is uniformly safe and callers don't
    /// branch on `Option`. [`ProcessGroup::kill_best_effort`] and
    /// `Drop` no-op when NULL.
    job: HANDLE,
}

#[cfg(not(any(unix, windows)))]
struct ProcessGroupInner {}

// SAFETY: `HANDLE` is `*mut c_void`. The Win32 Job-Object APIs are
// internally synchronized; transferring ownership of a Job HANDLE
// between threads is documented as safe. We never dereference the
// handle as a pointer.
#[cfg(windows)]
unsafe impl Send for ProcessGroupInner {}
#[cfg(windows)]
unsafe impl Sync for ProcessGroupInner {}

impl ProcessGroup {
    /// Configure `cmd` so the spawned child enters its own process
    /// group / Job Object. Call **before** `cmd.spawn()`. After spawn,
    /// call [`Self::attach`] with the resulting [`Child`].
    ///
    /// On Windows the helper also sets `CREATE_NO_WINDOW` on the
    /// child's creation flags via
    /// [`crate::hide_console_window_tokio`], so every provider-
    /// adapter spawn (the only callers of `ProcessGroup`) doesn't
    /// flash a cmd window on launch. Inherited by descendants the
    /// child itself spawns (npm forks, MCP proxies, etc.), which
    /// silences the whole subtree without per-grandchild fixes.
    pub fn before_spawn(cmd: &mut Command) -> Self {
        // Apply on every platform so the call site has uniform
        // behavior; the helper is a no-op on non-Windows.
        crate::windows_console::hide_console_window_tokio(cmd);

        #[cfg(unix)]
        {
            // SAFETY: `setpgid(0, 0)` is async-signal-safe per
            // POSIX.1-2017 and is the canonical use case for tokio's
            // `pre_exec` hook. Runs in the forked child between
            // `fork()` and `execve` — exactly the window we need to
            // make the child its own group leader.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            Self {
                inner: ProcessGroupInner { pgid: 0 },
            }
        }

        #[cfg(windows)]
        {
            // `cmd` itself isn't touched on Windows — the Job Object
            // can only accept the child *after* spawn. The handle
            // lives in `inner.job` until `Drop`.
            let _ = cmd;
            // SAFETY: every Win32 call below is documented FFI; we
            // pass valid arguments and check return values.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Self {
                        inner: ProcessGroupInner {
                            job: std::ptr::null_mut(),
                        },
                    };
                }
                // KILL_ON_JOB_CLOSE: when the LAST handle to the job
                // closes, every process still in the job is killed.
                // Combined with `AssignProcessToJobObject` in
                // `attach`, this makes the `Drop` impl below a single
                // `CloseHandle` that reaps the whole subtree even if
                // `kill_best_effort` was never called explicitly.
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    let _ = CloseHandle(job);
                    return Self {
                        inner: ProcessGroupInner {
                            job: std::ptr::null_mut(),
                        },
                    };
                }
                Self {
                    inner: ProcessGroupInner { job },
                }
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = cmd;
            Self {
                inner: ProcessGroupInner {},
            }
        }
    }

    /// Bind the just-spawned child to this group. Idempotent — calling
    /// twice has no extra effect. Returns `true` when the child is in
    /// the group. Returns `false` only on rare failures (child pid
    /// already reaped, Job Object creation failed earlier, or Win32
    /// `AssignProcessToJobObject` refused — which it can if the child
    /// was already assigned to a *different* job by a parent).
    pub fn attach(&mut self, child: &Child) -> bool {
        #[cfg(unix)]
        {
            if let Some(pid) = child.id() {
                if let Ok(p) = i32::try_from(pid) {
                    self.inner.pgid = p;
                    return true;
                }
            }
            false
        }

        #[cfg(windows)]
        {
            if self.inner.job.is_null() {
                return false;
            }
            // tokio exposes `Child::raw_handle()` on Windows — the
            // returned HANDLE is owned by the Child (we don't close
            // it). `AssignProcessToJobObject` only borrows it.
            let handle = match child.raw_handle() {
                Some(h) => h as HANDLE,
                None => return false,
            };
            // SAFETY: documented FFI; valid handles, return value
            // checked.
            let ok = unsafe { AssignProcessToJobObject(self.inner.job, handle) };
            ok != 0
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            false
        }
    }

    /// Send a best-effort termination signal to every member of the
    /// group. Unix sends `SIGTERM` (graceful — children may flush
    /// state); Windows calls `TerminateJobObject` (immediate — there
    /// is no per-job SIGTERM analogue). Idempotent, never panics.
    ///
    /// Callers typically invoke this in a `Drop` impl (or an idle-
    /// watchdog kill path) *before* calling `start_kill()` on the
    /// direct child, so grandchildren get the signal while the
    /// parent is still alive to propagate it.
    pub fn kill_best_effort(&self) {
        #[cfg(unix)]
        {
            // A pgid of 0 or negative is never something `attach`
            // would produce, but guard anyway: `killpg(0, sig)`
            // would broadcast SIGTERM to flowstate's *own* process
            // group per POSIX, which would kill us along with any
            // sibling tools sharing the group.
            if self.inner.pgid > 0 {
                // SAFETY: `killpg` is a standard POSIX syscall; a
                // valid positive pgid is the documented contract.
                // ESRCH (group already empty) is the expected race
                // on clean shutdown and silently ignored.
                unsafe {
                    libc::killpg(self.inner.pgid, libc::SIGTERM);
                }
            }
        }

        #[cfg(windows)]
        {
            if !self.inner.job.is_null() {
                // SAFETY: documented FFI; we own the handle until
                // `Drop` runs.
                unsafe {
                    TerminateJobObject(self.inner.job, 1);
                }
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            // No-op stub.
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if !self.inner.job.is_null() {
            // KILL_ON_JOB_CLOSE means closing the handle reaps any
            // members still alive. Errors are ignored — the kernel
            // runs teardown for us either way.
            unsafe {
                let _ = CloseHandle(self.inner.job);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn kill_rejects_uninitialised_groups() {
        // A freshly-built group has pgid = 0. `kill_best_effort`
        // would otherwise broadcast SIGTERM to the test runner's own
        // group, which is exactly the accident the pgid > 0 guard
        // prevents. The assertion is "we didn't suicide" — if the
        // guard regresses, the test binary dies before reporting.
        let group = ProcessGroup {
            inner: ProcessGroupInner { pgid: 0 },
        };
        group.kill_best_effort();
    }

    #[tokio::test]
    async fn setpgid_runs_before_exec() {
        // Spawn `sleep` in its own group and assert the child's pgid
        // equals its pid. With our pre_exec hook the child should be
        // its own group leader.
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg("5").kill_on_drop(true);
        let mut group = ProcessGroup::before_spawn(&mut cmd);
        let child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("pid") as i32;
        let attached = group.attach(&child);
        assert!(attached, "attach should succeed for a live child");
        let pgid = unsafe { libc::getpgid(pid) };
        assert_eq!(pgid, pid, "child should lead its own group");
        drop(child);
    }
}
