//! `install_cli` Tauri command — copies/symlinks the bundled
//! `flow` binary onto the user's PATH.
//!
//! The desktop app ships the `flow` binary alongside the main
//! `flowstate` binary inside the app bundle (see
//! `.github/workflows/build.yml` for the post-`tauri build` copy
//! step). This command resolves that bundled location, then makes
//! it discoverable from a terminal:
//!
//! - **macOS / Linux**:
//!   - `user_local` → symlink `~/.local/bin/flow` → bundled binary.
//!     No password prompt. Most modern shells include `~/.local/bin`
//!     in `$PATH`; if not, we report `on_path: false` and the UI
//!     surfaces the line the user needs to add to their rc file.
//!   - `system` → symlink `/usr/local/bin/flow` → bundled binary
//!     via `osascript` (mac) or `pkexec` (linux), prompting the OS
//!     for an admin password.
//!
//! - **Windows**:
//!   - `user_local` → copy `flow.exe` to
//!     `%LOCALAPPDATA%\Programs\flowstate\bin\flow.exe`, then
//!     prepend that directory to the user's `Path` env var via
//!     `HKCU\Environment` and broadcast `WM_SETTINGCHANGE` so new
//!     shells pick it up. No admin prompt.
//!   - `system` is unsupported on Windows in v1 — the Tauri
//!     command rejects it. The Settings UI hides the option.
//!
//! All install paths are idempotent: re-running over an existing
//! symlink/file that already points at the current binary is a
//! no-op success. An existing entry pointing at a different file
//! is overwritten (the user clicked Install — they want this one).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename of the bundled binary (no extension on Unix, `.exe`
/// on Windows). Matches the `[[bin]] name = "flow"` declaration
/// in `crates/cli/Cargo.toml`.
#[cfg(windows)]
const FLOW_BINARY_NAME: &str = "flow.exe";
#[cfg(not(windows))]
const FLOW_BINARY_NAME: &str = "flow";

/// Where on the filesystem the user wants the CLI installed.
///
/// `user_local`: per-user, no privilege prompt. The default and
/// recommended choice.
///
/// `system`: machine-wide. Triggers an OS password dialog on
/// macOS/Linux. Rejected on Windows (use `user_local`).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallTarget {
    UserLocal,
    System,
}

/// Outcome of an `install_cli` call surfaced to the Settings UI.
#[derive(Debug, Serialize)]
pub struct InstallCliReport {
    /// Where the symlink/copy was placed.
    pub installed_path: String,
    /// Path of the source binary the install points at.
    pub source_path: String,
    /// `true` if the install location's parent directory is on
    /// the user's `$PATH` (Unix) or the user's persistent `Path`
    /// (Windows). When `false`, the UI shows guidance for
    /// adding it manually.
    pub on_path: bool,
    /// Echoed back so the UI can label the result panel.
    pub target: InstallTarget,
}

/// Pre-mount status for the Settings UI to show on render.
/// `installed: false` means no `flow` symlink/file exists in
/// either of the two known locations; `installed_path` is `None`.
/// `installed: true` returns the location, plus
/// `points_at_current` indicating whether the existing entry
/// points at the same `source_path` we'd install today (a stale
/// link from a moved Flowstate.app would show `false`).
#[derive(Debug, Serialize)]
pub struct InstallCliStatus {
    pub installed: bool,
    pub installed_path: Option<String>,
    pub source_path: String,
    pub points_at_current: bool,
    /// `true` when the parent dir of the install path is on PATH.
    pub on_path: bool,
}

/// Resolve the bundled `flow` binary path. We resolve from
/// `current_exe()` (the running `flowstate` binary) and look for a
/// sibling `flow` / `flow.exe` in the same directory. That works
/// for both macOS app bundles (sibling under `Contents/MacOS/`)
/// and the cargo `target/<profile>/` dir during dev.
fn bundled_flow_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("resolve current_exe: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "current_exe has no parent dir".to_string())?;
    let candidate = dir.join(FLOW_BINARY_NAME);
    if !candidate.exists() {
        return Err(format!(
            "bundled flow binary not found at {} \
             (the desktop app bundle is missing the CLI — \
              this is a packaging bug, please report it)",
            candidate.display()
        ));
    }
    Ok(candidate)
}

#[cfg(unix)]
fn home_dir() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "could not resolve $HOME".to_string())
}

#[cfg(unix)]
fn user_local_bin() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".local").join("bin"))
}

/// Read `$PATH` and return whether `dir` is one of the entries.
/// We compare canonicalized paths so `~/.local/bin` and the
/// canonical `/Users/foo/.local/bin` match. Missing PATH entries
/// (deleted dirs) are silently skipped.
#[cfg(unix)]
fn dir_on_path(dir: &Path) -> bool {
    let want = match std::fs::canonicalize(dir) {
        Ok(p) => p,
        Err(_) => dir.to_path_buf(),
    };
    let path = match std::env::var_os("PATH") {
        Some(v) => v,
        None => return false,
    };
    for entry in std::env::split_paths(&path) {
        let canon = std::fs::canonicalize(&entry).unwrap_or(entry);
        if canon == want {
            return true;
        }
    }
    false
}

/// Replace `target` with a symlink to `source`. If `target` is
/// itself a symlink (regardless of where it points) we unlink it
/// and recreate. If it's a regular file we overwrite. Parents are
/// created as needed.
#[cfg(unix)]
fn force_symlink(source: &Path, target: &Path) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
    }
    // `symlink_metadata` does NOT follow symlinks — we want to
    // detect a stale broken symlink and remove it before creating
    // the new one. `metadata()` would error on a dangling link.
    if std::fs::symlink_metadata(target).is_ok() {
        std::fs::remove_file(target)
            .map_err(|e| format!("remove existing {}: {e}", target.display()))?;
    }
    std::os::unix::fs::symlink(source, target).map_err(|e| {
        format!(
            "symlink {} -> {}: {e}",
            target.display(),
            source.display()
        )
    })
}

/// Resolve the existing symlink target if `path` is a symlink,
/// otherwise the path itself if it's a regular file. Returns
/// `None` if nothing's there (or for any IO error reading the
/// link). Used by `installCli_status` to detect stale links.
#[cfg(unix)]
fn link_or_path(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        std::fs::read_link(path).ok()
    } else if meta.is_file() {
        Some(path.to_path_buf())
    } else {
        None
    }
}

#[cfg(unix)]
fn install_user_local(source: &Path) -> Result<InstallCliReport, String> {
    let bin_dir = user_local_bin()?;
    let target = bin_dir.join(FLOW_BINARY_NAME);
    force_symlink(source, &target)?;
    let on_path = dir_on_path(&bin_dir);
    Ok(InstallCliReport {
        installed_path: target.to_string_lossy().into_owned(),
        source_path: source.to_string_lossy().into_owned(),
        on_path,
        target: InstallTarget::UserLocal,
    })
}

/// macOS: `osascript -e 'do shell script "ln -sfn …" with administrator privileges'`.
/// Linux: `pkexec sh -c 'ln -sfn …'`.
#[cfg(unix)]
fn install_system(source: &Path) -> Result<InstallCliReport, String> {
    let target = PathBuf::from("/usr/local/bin").join(FLOW_BINARY_NAME);
    let source_str = source
        .to_str()
        .ok_or_else(|| "binary path contains non-UTF-8".to_string())?;
    let target_str = target
        .to_str()
        .ok_or_else(|| "install path contains non-UTF-8".to_string())?;

    // Both source and target paths are quoted to handle spaces.
    // `ln -sfn` is "symbolic, force-replace, no-deref-on-target" —
    // the no-deref bit is important: without it, replacing an
    // existing directory-shaped symlink follows into the directory
    // and creates the new link inside it.
    let shell_cmd = format!(
        "mkdir -p /usr/local/bin && ln -sfn {} {}",
        shell_escape(source_str),
        shell_escape(target_str)
    );

    #[cfg(target_os = "macos")]
    let output = {
        // AppleScript double-quotes the command and escapes inner
        // double quotes by doubling them. We've already shell-
        // escaped the paths to single-quote-safe forms, so the
        // outer AppleScript layer just needs to wrap the whole
        // thing.
        let osa = format!(
            "do shell script \"{}\" with administrator privileges",
            shell_cmd.replace('\\', "\\\\").replace('"', "\\\"")
        );
        std::process::Command::new(zenui_provider_api::resolve_cli_command("osascript"))
            .args(["-e", &osa])
            // Augment PATH so osascript itself resolves on machines
            // where the GUI launch's PATH is missing /usr/bin (rare,
            // but matches the rationale for the Settings escape hatch).
            .env("PATH", zenui_provider_api::path_with_extras(&[]))
            .output()
            .map_err(|e| format!("invoke osascript: {e}"))?
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let output = std::process::Command::new(zenui_provider_api::resolve_cli_command("pkexec"))
        .args(["sh", "-c", &shell_cmd])
        .env("PATH", zenui_provider_api::path_with_extras(&[]))
        .output()
        .map_err(|e| {
            format!(
                "invoke pkexec: {e} \
                 (install Polkit, or pick the Install for me option)"
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "system install failed (exit {}): {}",
            output.status,
            stderr.trim()
        ));
    }

    Ok(InstallCliReport {
        installed_path: target_str.to_string(),
        source_path: source_str.to_string(),
        // `/usr/local/bin` is on PATH on every default macOS /
        // Linux user shell config we know of. We don't probe to
        // confirm because shell init may not be loaded for the
        // process running this code.
        on_path: true,
        target: InstallTarget::System,
    })
}

/// Single-quote a string for safe inclusion in `sh -c '…'`. We
/// switch into double quotes inside any embedded `'` and back out.
#[cfg(unix)]
fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// ─── Windows ────────────────────────────────────────────────────

#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::RegKey;

    use super::{InstallCliReport, InstallTarget, FLOW_BINARY_NAME};

    pub fn install_dir() -> Result<PathBuf, String> {
        let local = std::env::var_os("LOCALAPPDATA")
            .ok_or_else(|| "LOCALAPPDATA not set".to_string())?;
        Ok(PathBuf::from(local)
            .join("Programs")
            .join("flowstate")
            .join("bin"))
    }

    pub fn install_user_local(source: &Path) -> Result<InstallCliReport, String> {
        let bin_dir = install_dir()?;
        std::fs::create_dir_all(&bin_dir)
            .map_err(|e| format!("create dir {}: {e}", bin_dir.display()))?;
        let target = bin_dir.join(FLOW_BINARY_NAME);
        // `copy` replaces the destination atomically on Windows
        // (NTFS) when the source is on the same volume — for our
        // case (both inside Program Files / LOCALAPPDATA) that's
        // typically the case. Even cross-volume, the worst case is
        // a brief window where `flow.exe` is partially written;
        // any concurrent invocation either sees the old or fails
        // to launch and the user retries. We don't try to atomic-
        // rename because Tauri builds rarely change the binary
        // signature mid-session.
        std::fs::copy(source, &target).map_err(|e| {
            format!("copy {} to {}: {e}", source.display(), target.display())
        })?;

        let on_path = ensure_user_path_includes(&bin_dir)?;
        if on_path {
            broadcast_environment_change();
        }
        Ok(InstallCliReport {
            installed_path: target.to_string_lossy().into_owned(),
            source_path: source.to_string_lossy().into_owned(),
            on_path,
            target: InstallTarget::UserLocal,
        })
    }

    /// Read the user's persistent `Path` from the registry. If
    /// `dir` isn't already present (case-insensitive comparison),
    /// prepend it. Returns whether `dir` is now on the persisted
    /// user PATH (always `true` after a successful write).
    fn ensure_user_path_includes(dir: &Path) -> Result<bool, String> {
        let dir_str = dir
            .to_str()
            .ok_or_else(|| "install dir contains non-UTF-16-safe chars".to_string())?;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let env = hkcu
            .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
            .map_err(|e| format!("open HKCU\\Environment: {e}"))?;
        let existing: String = env.get_value("Path").unwrap_or_default();
        let already = existing
            .split(';')
            .any(|p| p.trim().eq_ignore_ascii_case(dir_str));
        if already {
            return Ok(true);
        }
        let new_value = if existing.is_empty() {
            dir_str.to_string()
        } else {
            format!("{dir_str};{existing}")
        };
        env.set_value("Path", &new_value)
            .map_err(|e| format!("write HKCU\\Environment\\Path: {e}"))?;
        Ok(true)
    }

    /// Notify the system that environment variables have changed
    /// so newly-launched shells pick up the new PATH without a
    /// logout. Existing shells keep their old environment until
    /// restarted — that's a Windows quirk we can't paper over.
    fn broadcast_environment_change() {
        let env_str: Vec<u16> = "Environment\0".encode_utf16().collect();
        // SAFETY: `SendMessageTimeoutW` is documented as safe to
        // call from any thread; we pass HWND_BROADCAST which the
        // OS routes to top-level windows. SMTO_ABORTIFHUNG keeps
        // the call from blocking on a frozen receiver.
        unsafe {
            let mut result: usize = 0;
            SendMessageTimeoutW(
                HWND_BROADCAST as HWND,
                WM_SETTINGCHANGE,
                0 as WPARAM,
                env_str.as_ptr() as LPARAM,
                SMTO_ABORTIFHUNG,
                100,
                &mut result as *mut _ as *mut _,
            );
        }
    }

    /// Whether HKCU\Environment\Path currently contains `dir`.
    /// Used by the Settings UI to render the install status row.
    pub fn dir_on_user_path(dir: &Path) -> bool {
        let Ok(s) = dir.to_str().ok_or(()) else { return false };
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(env) = hkcu.open_subkey("Environment") else {
            return false;
        };
        let existing: String = env.get_value("Path").unwrap_or_default();
        existing
            .split(';')
            .any(|p| p.trim().eq_ignore_ascii_case(s))
    }
}

// ─── Tauri commands ─────────────────────────────────────────────

/// Install the bundled `flow` binary onto the user's PATH.
#[tauri::command]
pub async fn install_cli(target: InstallTarget) -> Result<InstallCliReport, String> {
    let source = bundled_flow_binary()?;

    #[cfg(unix)]
    {
        match target {
            InstallTarget::UserLocal => install_user_local(&source),
            InstallTarget::System => install_system(&source),
        }
    }

    #[cfg(windows)]
    {
        match target {
            InstallTarget::UserLocal => win::install_user_local(&source),
            InstallTarget::System => Err(
                "system-wide install is not supported on Windows in this version; \
                 use the per-user install instead"
                    .to_string(),
            ),
        }
    }
}

/// Probe the well-known install locations and report current
/// status. Called by the Settings UI on mount so the user sees
/// "Installed at /Users/foo/.local/bin/flow" or "Not installed."
#[tauri::command]
pub async fn install_cli_status() -> Result<InstallCliStatus, String> {
    let source = bundled_flow_binary()?;

    #[cfg(unix)]
    {
        // Probe `/usr/local/bin/flow` first (most discoverable),
        // then `~/.local/bin/flow`. We report the first one that
        // exists.
        let candidates = [
            PathBuf::from("/usr/local/bin").join(FLOW_BINARY_NAME),
            user_local_bin()?.join(FLOW_BINARY_NAME),
        ];
        for path in candidates {
            if let Some(target) = link_or_path(&path) {
                let resolved = std::fs::canonicalize(&target).unwrap_or(target.clone());
                let source_canon =
                    std::fs::canonicalize(&source).unwrap_or_else(|_| source.clone());
                let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
                return Ok(InstallCliStatus {
                    installed: true,
                    installed_path: Some(path.to_string_lossy().into_owned()),
                    source_path: source.to_string_lossy().into_owned(),
                    points_at_current: resolved == source_canon,
                    on_path: dir_on_path(&parent),
                });
            }
        }
        Ok(InstallCliStatus {
            installed: false,
            installed_path: None,
            source_path: source.to_string_lossy().into_owned(),
            points_at_current: false,
            on_path: false,
        })
    }

    #[cfg(windows)]
    {
        let bin_dir = win::install_dir()?;
        let path = bin_dir.join(FLOW_BINARY_NAME);
        if path.exists() {
            // Rough freshness check: copies have the same content,
            // not the same path, so we compare file lengths as a
            // weak signal for "is this the same build?". A diff
            // means an old install from a previous Flowstate
            // version — we still report installed: true but
            // points_at_current: false so the UI can offer Reinstall.
            let same = std::fs::metadata(&path)
                .ok()
                .zip(std::fs::metadata(&source).ok())
                .map(|(a, b)| a.len() == b.len())
                .unwrap_or(false);
            Ok(InstallCliStatus {
                installed: true,
                installed_path: Some(path.to_string_lossy().into_owned()),
                source_path: source.to_string_lossy().into_owned(),
                points_at_current: same,
                on_path: win::dir_on_user_path(&bin_dir),
            })
        } else {
            Ok(InstallCliStatus {
                installed: false,
                installed_path: None,
                source_path: source.to_string_lossy().into_owned(),
                points_at_current: false,
                on_path: false,
            })
        }
    }
}
