//! Cross-platform CLI binary resolution shared by every provider that
//! shells out to an external tool (claude, codex, copilot, ...).
//!
//! Why this exists: the obvious approach of `Command::new("copilot")`
//! relies on Rust's `process::Command` doing PATH lookup, which works
//! on Linux/macOS and (mostly) on Windows — but it fails the moment a
//! provider needs to *validate* the binary's location before spawning
//! (e.g. to log it, to feed it to a JS bridge that itself does
//! `existsSync(path)`, or to surface a useful "not found" diagnostic
//! instead of a generic ENOENT). It also fails when the host process
//! doesn't inherit a useful PATH (cron, systemd units, IDE-spawned
//! children, embedded daemons under sandboxes), in which case we want
//! to fall back to well-known install locations across the three OSes.
//!
//! [`find_cli_binary`] handles both: a real PATH walk in pure Rust
//! (so we get correct PATHEXT handling on Windows without spawning
//! `where`), then a platform-specific list of common install
//! locations. Returns the first hit as an absolute `PathBuf`, or
//! `None` if nothing exists.

use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

/// Process-wide list of additional search directories the user has
/// configured under the `binaries.search_paths` user_config key.
/// Read by [`find_cli_binary`] right after the PATH walk and before
/// the platform fallbacks, so a user who has `claude` (or any other
/// provider CLI) in a non-standard location can point flowstate at
/// it without rebuilding.
///
/// `OnceLock<RwLock<...>>` rather than a plain `RwLock<Vec<...>>`
/// because the inner storage can't be a `const` (PathBuf is heap-
/// allocated), and `Mutex::new(Vec::new()).const_new()` isn't stable
/// for our MSRV.
static EXTRA_SEARCH_PATHS: OnceLock<RwLock<Vec<PathBuf>>> = OnceLock::new();

fn extra_search_paths_lock() -> &'static RwLock<Vec<PathBuf>> {
    EXTRA_SEARCH_PATHS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Replace the process-wide list of user-configured extra search
/// directories. The Tauri shell reads `binaries.search_paths` from
/// `UserConfigStore` at startup and on every settings-page write,
/// then calls this with the parsed paths. Empty entries are ignored
/// (defensive against trim-whitespace mishaps in the UI).
///
/// Idempotent and inexpensive — replaces the inner Vec under a
/// short-lived write lock.
pub fn set_extra_search_paths(paths: Vec<PathBuf>) {
    let lock = extra_search_paths_lock();
    let cleaned: Vec<PathBuf> = paths
        .into_iter()
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    match lock.write() {
        Ok(mut guard) => {
            *guard = cleaned;
        }
        Err(poisoned) => {
            // Rare — only happens if a previous writer panicked
            // while holding the lock. Recover the inner Vec and
            // continue; we don't want a transient panic to lock
            // out future config updates.
            let mut guard = poisoned.into_inner();
            *guard = cleaned;
        }
    }
}

/// Read-only snapshot of the configured extra search directories.
/// Exposed primarily for diagnostic / "verify-config" surfaces;
/// [`find_cli_binary`] consults the live state directly.
pub fn extra_search_paths() -> Vec<PathBuf> {
    let lock = extra_search_paths_lock();
    match lock.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Locate a CLI binary by name across PATH, user-configured extra
/// search directories, and a curated list of well-known install
/// locations.
///
/// # Resolution order
///
/// 1. **PATH walk** — splits `$PATH` (`%PATH%` on Windows) by the
///    platform path separator and checks each entry. On Windows the
///    walk also tries every extension in `%PATHEXT%` (or the standard
///    `.COM/.EXE/.BAT/.CMD` set if PATHEXT isn't set).
/// 2. **User-configured search paths** — directories the user added
///    under the `binaries.search_paths` key in `UserConfigStore`.
///    Pushed to this resolver via [`set_extra_search_paths`] at
///    daemon startup and on every settings-page write. Acts as the
///    explicit escape hatch when neither PATH nor the curated
///    fallbacks find the binary.
/// 3. **Platform fallbacks** — [`platform_fallbacks`] enumerates npm
///    globals, user-local bin dirs, common version-manager shims,
///    and system package-manager shims. Designed to catch the case
///    where a Tauri / GUI launch inherits a stripped PATH compared
///    to what the user's shell sees.
///
/// Returns `None` when nothing matches. Callers should surface a
/// clear "install <name> and ensure it's on PATH" error.
pub fn find_cli_binary(name: &str) -> Option<PathBuf> {
    if let Some(path) = walk_path_for_binary(name) {
        return Some(path);
    }

    // User-configured directories — same extension-walk logic as the
    // PATH branch so a Windows entry like `C:\tools\` resolves
    // `claude.cmd` as well as bare `claude`. Empty paths are filtered
    // by `set_extra_search_paths` already, but we re-check `is_dir`
    // each call so a path the user removed since startup doesn't
    // crash with `ENOENT`-flavored panics.
    let extras = extra_search_paths();
    let extensions = executable_extensions();
    for dir in &extras {
        if dir.as_os_str().is_empty() || !dir.is_dir() {
            continue;
        }
        for ext in &extensions {
            let candidate = if ext.is_empty() {
                dir.join(name)
            } else {
                let mut name_with_ext = std::ffi::OsString::from(name);
                name_with_ext.push(ext);
                dir.join(name_with_ext)
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    for candidate in platform_fallbacks(name) {
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

fn walk_path_for_binary(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let extensions = executable_extensions();

    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in &extensions {
            let mut candidate = dir.join(name);
            if !ext.is_empty() {
                let mut name_with_ext = std::ffi::OsString::from(name);
                name_with_ext.push(ext);
                candidate = dir.join(name_with_ext);
            }
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// On Windows, try every extension in PATHEXT (or a sensible default)
/// so a bare `copilot` lookup matches `copilot.exe`, `copilot.cmd`,
/// etc. On POSIX systems the only "extension" is the empty string —
/// Linux and macOS don't decorate executables with suffixes.
fn executable_extensions() -> Vec<String> {
    if cfg!(windows) {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut exts: Vec<String> = pathext
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect();
        // Always include the bare name so `copilot` itself is checked
        // even on Windows (e.g. for shim scripts without an extension).
        exts.insert(0, String::new());
        exts
    } else {
        vec![String::new()]
    }
}

fn platform_fallbacks(name: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();

    if cfg!(windows) {
        // Tauri / GUI-launched processes on Windows inherit PATH from
        // the launcher (explorer.exe, the Start menu), NOT from the
        // user's shell rc. So directories like `~/.local/bin` and
        // `%LOCALAPPDATA%\Volta\bin` that PowerShell would normally
        // see are typically absent from a Tauri app's PATH. Most of
        // the entries below cover that gap.
        let userprofile = std::env::var_os("USERPROFILE");
        let local_app_data = std::env::var_os("LOCALAPPDATA");
        let app_data = std::env::var_os("APPDATA");
        let program_data = std::env::var_os("PROGRAMDATA");

        // npm global installs — the most common path for `claude`,
        // `codex`, etc. Both the default location and a few common
        // overrides are covered.
        if let Some(ref appdata) = app_data {
            for ext in ["cmd", "exe", "ps1"] {
                paths.push(
                    PathBuf::from(appdata)
                        .join("npm")
                        .join(format!("{name}.{ext}")),
                );
            }
        }
        if let Some(ref localappdata) = local_app_data {
            for ext in ["cmd", "exe"] {
                paths.push(
                    PathBuf::from(localappdata)
                        .join("npm")
                        .join(format!("{name}.{ext}")),
                );
            }
        }

        // User-local bin dir — pip installs, cargo install, manual
        // drops. Mirrors `~/.local/bin` on Unix.
        if let Some(ref home) = userprofile {
            for ext in ["exe", "cmd", "bat", ""] {
                let leaf = if ext.is_empty() {
                    name.to_string()
                } else {
                    format!("{name}.{ext}")
                };
                paths.push(
                    PathBuf::from(home)
                        .join(".local")
                        .join("bin")
                        .join(leaf),
                );
            }
        }

        // Volta — Node.js + tool version manager. Uses fixed shim
        // exes under `%LOCALAPPDATA%\Volta\bin`.
        if let Some(ref localappdata) = local_app_data {
            paths.push(
                PathBuf::from(localappdata)
                    .join("Volta")
                    .join("bin")
                    .join(format!("{name}.exe")),
            );
        }

        // Scoop — popular user-mode package manager. Shims live
        // under `~\scoop\shims`.
        if let Some(ref home) = userprofile {
            for ext in ["cmd", "exe"] {
                paths.push(
                    PathBuf::from(home)
                        .join("scoop")
                        .join("shims")
                        .join(format!("{name}.{ext}")),
                );
            }
        }

        // Chocolatey — system-wide package manager. Shims live in
        // `%PROGRAMDATA%\chocolatey\bin`.
        if let Some(ref pd) = program_data {
            for ext in ["exe", "cmd"] {
                paths.push(
                    PathBuf::from(pd)
                        .join("chocolatey")
                        .join("bin")
                        .join(format!("{name}.{ext}")),
                );
            }
        }

        // bun — Bun's global bin dir. Some npm-installed tools end
        // up here when the user's `npm` is bun-shimmed.
        if let Some(ref home) = userprofile {
            for ext in ["exe", "cmd"] {
                paths.push(
                    PathBuf::from(home)
                        .join(".bun")
                        .join("bin")
                        .join(format!("{name}.{ext}")),
                );
            }
        }

        // pnpm — its global binary dir defaults to
        // `%LOCALAPPDATA%\pnpm` on Windows but the user can move it
        // via `%PNPM_HOME%` (the new convention) or via `pnpm config
        // set global-bin-dir`. We check both the env-overridable path
        // first (if set) and the default location.
        if let Some(pnpm_home) = std::env::var_os("PNPM_HOME") {
            for ext in ["exe", "cmd"] {
                paths.push(
                    PathBuf::from(&pnpm_home).join(format!("{name}.{ext}")),
                );
            }
        }
        if let Some(ref localappdata) = local_app_data {
            for ext in ["exe", "cmd"] {
                paths.push(
                    PathBuf::from(localappdata)
                        .join("pnpm")
                        .join(format!("{name}.{ext}")),
                );
            }
        }

        // yarn (Berry / v2+) — global bin dir on Windows.
        if let Some(ref localappdata) = local_app_data {
            paths.push(
                PathBuf::from(localappdata)
                    .join("Yarn")
                    .join("bin")
                    .join(format!("{name}.cmd")),
            );
        }
        // yarn (classic / v1) — older global install layout under
        // `%LOCALAPPDATA%\Yarn\config\global\node_modules\.bin`.
        if let Some(ref localappdata) = local_app_data {
            paths.push(
                PathBuf::from(localappdata)
                    .join("Yarn")
                    .join("config")
                    .join("global")
                    .join("node_modules")
                    .join(".bin")
                    .join(format!("{name}.cmd")),
            );
        }

        // winget — Microsoft's package manager creates user-mode
        // shims at `%LOCALAPPDATA%\Microsoft\WinGet\Links` (added
        // ~v1.6, late 2023). Many users install dev tooling via
        // `winget install ...` and never realize the shim dir isn't
        // on the GUI-launch PATH by default.
        if let Some(ref localappdata) = local_app_data {
            for ext in ["exe", "cmd"] {
                paths.push(
                    PathBuf::from(localappdata)
                        .join("Microsoft")
                        .join("WinGet")
                        .join("Links")
                        .join(format!("{name}.{ext}")),
                );
            }
        }

        // Standard Program Files installations.
        paths.push(PathBuf::from(format!(
            "C:\\Program Files\\{name}\\{name}.exe"
        )));
        paths.push(PathBuf::from(format!(
            "C:\\Program Files (x86)\\{name}\\{name}.exe"
        )));

        // Per-user Programs install dir (Tauri / Electron / npm
        // user-mode apps).
        if let Some(ref localappdata) = local_app_data {
            paths.push(
                PathBuf::from(localappdata)
                    .join("Programs")
                    .join(name)
                    .join(format!("{name}.exe")),
            );
        }
    } else {
        // POSIX hosts. Most version managers (nvm, fnm, mise, asdf)
        // shim via PATH rather than fixed install paths, so the PATH
        // walk above usually catches them. The entries below cover
        // tools that DO drop a fixed file:
        if let Some(home) = std::env::var_os("HOME") {
            // ~/.local/bin — pip user-installs, cargo install, npm
            // with `--prefix=$HOME/.local`.
            paths.push(PathBuf::from(&home).join(".local").join("bin").join(name));
            // ~/.bun/bin — Bun's global bin dir.
            paths.push(PathBuf::from(&home).join(".bun").join("bin").join(name));
            // ~/.volta/bin — Volta on POSIX.
            paths.push(PathBuf::from(&home).join(".volta").join("bin").join(name));
            // Custom npm prefix some setups use to avoid `sudo npm`.
            paths.push(
                PathBuf::from(&home)
                    .join(".npm-global")
                    .join("bin")
                    .join(name),
            );
            // pnpm global bin dir. POSIX default is the XDG data
            // dir (`~/.local/share/pnpm`); the env-var override
            // takes precedence below.
            paths.push(
                PathBuf::from(&home)
                    .join(".local")
                    .join("share")
                    .join("pnpm")
                    .join(name),
            );
            // yarn (classic / v1) global bin.
            paths.push(PathBuf::from(&home).join(".yarn").join("bin").join(name));
            // yarn (classic) — alternate `--global-folder` default.
            paths.push(
                PathBuf::from(&home)
                    .join(".config")
                    .join("yarn")
                    .join("global")
                    .join("node_modules")
                    .join(".bin")
                    .join(name),
            );
        }
        // Honor `$PNPM_HOME` if the user moved their pnpm global
        // bin elsewhere (the recommended pnpm config nowadays).
        if let Some(pnpm_home) = std::env::var_os("PNPM_HOME") {
            paths.push(PathBuf::from(&pnpm_home).join(name));
        }
        paths.push(PathBuf::from(format!("/opt/homebrew/bin/{name}")));
        paths.push(PathBuf::from(format!("/usr/local/bin/{name}")));
        paths.push(PathBuf::from(format!(
            "/home/linuxbrew/.linuxbrew/bin/{name}"
        )));
        paths.push(PathBuf::from(format!("/usr/bin/{name}")));
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_known_binary_via_path() {
        // `sh` exists on every POSIX system at /bin/sh and should
        // always be on PATH; on Windows we fall back to `cmd` which
        // ships in System32.
        let name = if cfg!(windows) { "cmd" } else { "sh" };
        let resolved = find_cli_binary(name);
        assert!(
            resolved.is_some(),
            "expected to resolve `{name}` via PATH walk"
        );
        let path = resolved.unwrap();
        assert!(
            path.is_absolute(),
            "resolved path must be absolute: {path:?}"
        );
        assert!(
            path.is_file(),
            "resolved path must point to a file: {path:?}"
        );
    }

    #[test]
    fn returns_none_for_nonexistent_binary() {
        assert!(find_cli_binary("definitely-not-a-real-binary-xyz123").is_none());
    }

    #[test]
    fn extensions_list_includes_empty_string() {
        // The empty string entry is what makes the bare-name check
        // work on every platform — without it, POSIX would skip the
        // join entirely and Windows shims without an extension would
        // be missed.
        let exts = executable_extensions();
        assert!(exts.contains(&String::new()));
    }

    #[test]
    fn platform_fallbacks_are_substituted() {
        let fallbacks = platform_fallbacks("copilot");
        let any_contains_copilot = fallbacks
            .iter()
            .any(|p| p.to_string_lossy().contains("copilot"));
        assert!(
            any_contains_copilot,
            "expected platform fallbacks to mention the binary name"
        );
    }

    /// Smoke test that each platform's fallback list mentions the
    /// directories we actually care about — so a careless edit that
    /// drops one of them gets caught at PR time. Done with substring
    /// matches because the actual paths get joined with platform-
    /// specific separators we don't want to recompute here.
    #[test]
    fn platform_fallbacks_cover_known_dirs() {
        let fallbacks = platform_fallbacks("claude");
        let combined: String = fallbacks
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        if cfg!(windows) {
            // Windows GUI-launched processes inherit a stripped PATH
            // compared to the user's shell, so the resolver must
            // know about every common per-user install location.
            // If a refactor drops any of these, flowstate.exe will
            // silently fail to find provider CLIs again.
            for needle in [
                "Volta",
                "scoop",
                "chocolatey",
                ".local",
                ".bun",
                "npm",
                "pnpm",
                "Yarn",
                "WinGet",
            ] {
                assert!(
                    combined.contains(needle),
                    "expected Windows fallbacks to include `{needle}`; got:\n{combined}"
                );
            }
        } else {
            for needle in [
                ".local",
                ".bun",
                ".volta",
                ".npm-global",
                "homebrew",
                "pnpm",
                ".yarn",
            ] {
                assert!(
                    combined.contains(needle),
                    "expected POSIX fallbacks to include `{needle}`; got:\n{combined}"
                );
            }
        }
    }
}
