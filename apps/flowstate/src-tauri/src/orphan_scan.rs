//! Startup orphan scan — reaps leftover subprocesses from a prior
//! flowstate that was SIGKILL'd before its Drop code could run.
//!
//! # Why this exists
//!
//! Two Unix child-process classes outlive their creator when
//! flowstate is SIGKILL'd (routine during `tauri dev` hot-reload):
//!
//! - `opencode serve`: spawned directly by flowstate. `kill_on_drop`
//!   only fires when `Drop::drop()` runs, which SIGKILL prevents.
//!   The child reparents to PID 1 and keeps running, holding its
//!   port and its basic-auth password.
//! - `flowstate mcp-server`: spawned by the *agent* (opencode /
//!   claude-cli / codex / copilot), grandchild of flowstate. When the
//!   agent dies too it reparents to PID 1. Under `tauri dev` the
//!   agent and its mcp-server both typically survive because they're
//!   detached from the flowstate process group.
//!
//! Both types of stale process hold a bad model of the world: they
//! point at the dead flowstate's loopback port. New tool calls hang
//! against them, which is what the user observed (a 3rd orchestration
//! turn stalling at 4+ minutes with no progress).
//!
//! The cure: before the next flowstate binds its own port, walk every
//! running process and SIGTERM any that match our signature AND have
//! reparented to PID 1. The parent-PID filter is critical — a sibling
//! `opencode serve` someone ran in another terminal is legitimate and
//! must NOT be killed.
//!
//! # Defence-in-depth
//!
//! This is layered with two other mitigations and any one is
//! sufficient on its own:
//!
//! 1. `OpenCodeServer::Drop` sends `killpg(pgid, SIGTERM)` on graceful
//!    shutdown — handles the common SIGTERM / window-close path.
//! 2. The flowstate `mcp-server` subcommand runs a parent-watchdog
//!    loop that self-exits within ~2 s when its parent flowstate dies
//!    (see `crates/core/mcp-server/src/lib.rs::spawn_parent_watchdog`).
//! 3. *This* scan — the only defence against the SIGKILL case where
//!    nothing in flowstate's own address space gets to run.
//!
//! # Matching rules
//!
//! A process is an orphan-reap candidate when **all** of:
//!
//! - It has reparented to init (`parent == Some(Pid(1))`). This is
//!   the Unix signal that the original parent is gone. Never kill a
//!   process with a live parent — it belongs to someone else.
//! - Its command line matches one of:
//!   - `opencode serve --hostname 127.0.0.1 --port N` (opencode spawn
//!     signature from `provider-opencode::server::spawn`).
//!   - `flowstate mcp-server …` (the argv shape flowstate uses for
//!     its own stdio proxy subcommand).
//!
//! SIGTERM (not SIGKILL) is sent first: opencode handles it cleanly
//! and flushes logs; the mcp-server has a parent-watchdog so it's
//! likely already exiting. If a SIGTERM'd process is still alive 2 s
//! later we don't escalate — waiting until the next flowstate
//! restart is fine; the scan runs again then.

use std::time::Duration;

use sysinfo::{Pid, System};
use tracing::{debug, info, warn};

/// Run the orphan scan. Returns the number of processes SIGTERM'd.
///
/// Safe to call on any platform. On non-Unix builds this is a no-op
/// that returns 0 — we have no `flowstate` Windows build today and
/// Windows process reparenting semantics differ enough that the same
/// filter wouldn't be correct.
///
/// Called once very early in the Tauri setup closure, before the
/// loopback HTTP binds (so a stale server can't accidentally serve
/// traffic for the new flowstate). Runs synchronously on the current
/// thread — the enumeration is a few milliseconds on a typical
/// desktop process table.
pub fn reap_orphaned_subprocesses() -> usize {
    #[cfg(unix)]
    {
        reap_unix()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(unix)]
fn reap_unix() -> usize {
    // `System::new_all` is heavier than we need (pulls CPU / memory
    // stats too), but the simpler API matches sysinfo 0.32's public
    // surface cleanly. The scan runs once at startup; the extra ms is
    // not a hot-path concern and avoids depending on private detail
    // of the refresh-kinds API across sysinfo point releases.
    let mut system = System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let init_pid = Pid::from_u32(1);
    let mut reaped = 0usize;

    for (pid, proc) in system.processes() {
        // Never self-terminate. Shouldn't be reachable (our parent
        // is the terminal/launchd, not PID 1, at startup) but cheap
        // insurance against a pathological env.
        if pid.as_u32() == std::process::id() {
            continue;
        }
        // Only reap processes whose direct parent is init. Siblings
        // spawned in other terminals by a different user session are
        // out of scope.
        if proc.parent() != Some(init_pid) {
            continue;
        }
        let cmd: Vec<String> = proc
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        if !is_orphan_signature(&cmd) {
            continue;
        }
        let pid_u32 = pid.as_u32();
        info!(
            pid = pid_u32,
            cmd = %cmd.join(" "),
            "reaping orphaned flowstate subprocess from prior run"
        );
        // Safety: `libc::kill` with a valid pid is a standard POSIX
        // syscall. EPERM (unlikely — same user) and ESRCH (raced and
        // already exited) are both fine to ignore here.
        let rc = unsafe { libc::kill(pid_u32 as libc::pid_t, libc::SIGTERM) };
        if rc == 0 {
            reaped += 1;
        } else {
            let err = std::io::Error::last_os_error();
            warn!(pid = pid_u32, %err, "SIGTERM to orphaned subprocess failed");
        }
    }

    // Tiny delay so an opencode that accepted SIGTERM has a chance to
    // release its port before the new flowstate tries to bind on a
    // freshly-randomised port. 100 ms is overkill for the happy path
    // and below any human-perceptible startup delay.
    if reaped > 0 {
        std::thread::sleep(Duration::from_millis(100));
        debug!(reaped, "orphan scan complete");
    }
    reaped
}

/// Return true if an argv vector matches a flowstate-spawned
/// subprocess signature. Hand-rolled rather than regex to keep
/// dependencies light — these are fixed argv patterns we control.
#[cfg(unix)]
fn is_orphan_signature(cmd: &[String]) -> bool {
    // `flowstate mcp-server …` — our own subcommand. Match on the
    // subcommand name appearing as argv[1] (exe path varies across
    // installs: /Applications/flowstate.app/.../flowstate, cargo
    // target paths, etc.).
    if cmd
        .get(1)
        .map(|s| s.as_str() == "mcp-server")
        .unwrap_or(false)
    {
        return true;
    }
    // `opencode serve …` — opencode ships a single `opencode` binary
    // whose first argument selects a subcommand. We match on
    // argv[0] basename `opencode` (any path) + argv[1] `serve`.
    let exe_basename = cmd
        .first()
        .and_then(|s| std::path::Path::new(s).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if exe_basename == "opencode"
        && cmd.get(1).map(|s| s.as_str() == "serve").unwrap_or(false)
    {
        return true;
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::is_orphan_signature;

    #[test]
    fn matches_flowstate_mcp_server() {
        let cmd = vec![
            "/Applications/flowstate.app/Contents/MacOS/flowstate".to_string(),
            "mcp-server".to_string(),
            "--http-base".to_string(),
            "http://127.0.0.1:4873".to_string(),
            "--session-id".to_string(),
            "s1".to_string(),
        ];
        assert!(is_orphan_signature(&cmd));
    }

    #[test]
    fn matches_opencode_serve() {
        let cmd = vec![
            "/opt/homebrew/bin/opencode".to_string(),
            "serve".to_string(),
            "--hostname".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "49192".to_string(),
        ];
        assert!(is_orphan_signature(&cmd));
    }

    #[test]
    fn rejects_unrelated_processes() {
        // Common non-flowstate processes that should not be reaped.
        let node = vec![
            "/usr/local/bin/node".to_string(),
            "/some/script.js".to_string(),
        ];
        assert!(!is_orphan_signature(&node));

        let opencode_without_serve = vec![
            "/opt/homebrew/bin/opencode".to_string(),
            "auth".to_string(),
            "login".to_string(),
        ];
        assert!(!is_orphan_signature(&opencode_without_serve));

        let flowstate_bare = vec![
            "/Applications/flowstate.app/Contents/MacOS/flowstate".to_string(),
        ];
        assert!(!is_orphan_signature(&flowstate_bare));

        let empty: Vec<String> = vec![];
        assert!(!is_orphan_signature(&empty));
    }
}
