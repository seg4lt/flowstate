//! Cross-platform helpers for suppressing the console window that
//! Windows allocates for every child process spawned from a GUI host.
//!
//! # Why this exists
//!
//! When a Windows GUI process (Tauri / Electron / Win32 app — anything
//! without an attached console) spawns a console application via
//! `CreateProcess`, the OS allocates a fresh console for the child so
//! its stdio has somewhere to go. That console is a real window and
//! flashes on screen for the lifetime of the spawn — even just a
//! 50ms `git rev-parse` produces a visible flicker, and a long-lived
//! provider CLI keeps a cmd window open the entire session.
//!
//! Setting `CREATE_NO_WINDOW` (`0x0800_0000`) in the creation flags
//! tells `CreateProcess` to use a hidden console for the child's
//! stdio. The flag is inherited by descendants unless they
//! explicitly request a console (`AllocConsole`), so suppressing it
//! at the top of our spawn tree (the bridge / CLI we own) silences
//! every grandchild npm fork, subshell, MCP proxy, etc. that those
//! processes go on to spawn.
//!
//! Two functions because [`std::process::Command`] and
//! [`tokio::process::Command`] are distinct types and `creation_flags`
//! lives on a different extension trait for each. Both are no-ops on
//! non-Windows hosts so callers don't need to cfg-gate.

/// Hide the console window for a [`tokio::process::Command`] before
/// spawn. Idempotent (repeated calls overwrite the same bit field).
/// No-op on non-Windows targets.
///
/// Most callers don't need to invoke this directly: the shared
/// [`crate::ProcessGroup::before_spawn`] runs it automatically for
/// every provider-adapter subprocess. Use it explicitly for one-off
/// `Command::new(...)` paths that don't go through `ProcessGroup`
/// (probe-style `--version` checks, Tauri command handlers shelling
/// out to `git` synchronously, the editor-launch helper, etc.).
pub fn hide_console_window_tokio(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        // Avoid pulling in winapi just for one constant.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // SAFETY: `creation_flags` is a documented Windows-only
        // method on `CommandExt`; passing a u32 bitfield is the
        // entire contract.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Same as [`hide_console_window_tokio`] but for the synchronous
/// [`std::process::Command`] used by code that doesn't need tokio
/// (build scripts, pure-`std` git wrappers, the install-CLI paths).
/// No-op on non-Windows targets.
pub fn hide_console_window_std(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}
