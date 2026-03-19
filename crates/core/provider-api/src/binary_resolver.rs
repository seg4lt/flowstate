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

/// Locate a CLI binary by name across PATH and a curated list of
/// well-known install locations.
///
/// # Resolution order
///
/// 1. **PATH walk** — splits `$PATH` (`%PATH%` on Windows) by the
///    platform path separator and checks each entry. On Windows the
///    walk also tries every extension in `%PATHEXT%` (or the standard
///    `.COM/.EXE/.BAT/.CMD` set if PATHEXT isn't set).
/// 2. **Platform fallbacks** — if PATH lookup misses, tries:
///    - **Linux/macOS**: `~/.local/bin/<name>`, `/opt/homebrew/bin/<name>`,
///      `/usr/local/bin/<name>`, `/home/linuxbrew/.linuxbrew/bin/<name>`,
///      `/usr/bin/<name>`
///    - **Windows**: `%LOCALAPPDATA%\Programs\<name>\<name>.exe`,
///      `%APPDATA%\npm\<name>.cmd`, `C:\Program Files\<name>\<name>.exe`
///
/// Returns `None` when nothing matches. Callers should surface a
/// clear "install <name> and ensure it's on PATH" error.
pub fn find_cli_binary(name: &str) -> Option<PathBuf> {
    if let Some(path) = walk_path_for_binary(name) {
        return Some(path);
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
        let pathext = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
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
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            paths.push(
                PathBuf::from(&local_app_data)
                    .join("Programs")
                    .join(name)
                    .join(format!("{name}.exe")),
            );
        }
        if let Some(app_data) = std::env::var_os("APPDATA") {
            paths.push(PathBuf::from(&app_data).join("npm").join(format!("{name}.cmd")));
            paths.push(PathBuf::from(&app_data).join("npm").join(format!("{name}.exe")));
        }
        paths.push(PathBuf::from(format!("C:\\Program Files\\{name}\\{name}.exe")));
        paths.push(PathBuf::from(format!("C:\\Program Files (x86)\\{name}\\{name}.exe")));
    } else {
        if let Some(home) = std::env::var_os("HOME") {
            paths.push(PathBuf::from(&home).join(".local").join("bin").join(name));
        }
        paths.push(PathBuf::from(format!("/opt/homebrew/bin/{name}")));
        paths.push(PathBuf::from(format!("/usr/local/bin/{name}")));
        paths.push(PathBuf::from(format!("/home/linuxbrew/.linuxbrew/bin/{name}")));
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
        assert!(path.is_absolute(), "resolved path must be absolute: {path:?}");
        assert!(path.is_file(), "resolved path must point to a file: {path:?}");
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
}
