//! `flow` — the Flowstate command-line companion.
//!
//! Single command:
//!
//! ```text
//! flow [PATH]
//! ```
//!
//! Resolves `PATH` (default `.`) to an absolute path, locates the
//! running Flowstate desktop app via its handshake file under the
//! OS app-data directory, and POSTs the path to the app's loopback
//! HTTP server. The desktop app then either reuses the matching
//! project or creates one, starts a new thread on it using the
//! user's saved default provider/model/effort/permission_mode, and
//! brings its window to the front.
//!
//! The CLI does NOT launch the app: if Flowstate isn't running we
//! print a clear message and exit non-zero. Auto-launch would mean
//! teaching the CLI where the app binary lives on every platform
//! and dealing with cold-start ordering — neither problem we want
//! the CLI to own. The user opens the app once; from then on `flow`
//! is the natural way to start work on a project.
//!
//! # Discovery
//!
//! The desktop app writes `<app_data_dir>/daemon.handshake` on
//! every launch (see `apps/flowstate/src-tauri/src/loopback_http.rs`).
//! It contains `{ base_url, pid, schema_version, build_sha }`. We
//! resolve `app_data_dir` cross-platform to match Tauri's own
//! `app_data_dir()`:
//!
//! - macOS:   `~/Library/Application Support/com.seg4lt.flowstate/`
//! - Linux:   `~/.local/share/com.seg4lt.flowstate/`         (XDG)
//! - Windows: `%APPDATA%\com.seg4lt.flowstate\`              (Roaming)
//!
//! The `dirs` crate's `data_dir()` returns the parent of the
//! bundle-id directory on every supported platform, so we just
//! append `com.seg4lt.flowstate` and `daemon.handshake`.
//!
//! # Liveness check
//!
//! The handshake file isn't deleted on app exit (the OS reclaims it
//! on process death, but if the process was SIGKILL'd or crashed
//! the file lingers with a stale `pid` and a port nothing's
//! listening on). Before posting we cheap-check whether `pid` is
//! still alive — `kill(pid, 0)` on Unix, `OpenProcess(SYNCHRONIZE,
//! …)` on Windows — and bail with the same "not running" message
//! if it isn't. Without the check the user would see a confusing
//! `Connection refused` from `ureq` instead.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// Tauri bundle identifier from `apps/flowstate/src-tauri/tauri.conf.json`.
/// Forms the directory name under each OS's app-data root. Keep in
/// sync with the `identifier` field in `tauri.conf.json` — they
/// must match or the CLI won't find the handshake file.
const BUNDLE_ID: &str = "com.seg4lt.flowstate";

/// Subset of the handshake JSON we care about. `build_sha` and
/// `schema_version` are present but irrelevant for the open-project
/// path; we tolerate extra fields for forward-compat.
#[derive(Debug, Deserialize)]
struct Handshake {
    base_url: String,
    pid: u32,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("flow: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("flow {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.len() > 1 {
        bail!("expected at most one path argument; got {}", args.len());
    }
    let raw_path = args.first().map(String::as_str).unwrap_or(".");

    let path = resolve_dir(raw_path)
        .with_context(|| format!("resolving path '{raw_path}'"))?;

    let handshake_path = handshake_file_path()
        .context("locating Flowstate handshake file")?;
    let handshake = read_handshake(&handshake_path)?;

    if !pid_alive(handshake.pid) {
        bail!(
            "Flowstate is not running. Launch the app and try again.\n\
             (handshake file at {} has stale pid {})",
            handshake_path.display(),
            handshake.pid,
        );
    }

    open_project(&handshake.base_url, &path)?;
    println!("Opened {} in Flowstate.", path.display());
    Ok(())
}

fn print_help() {
    let bin = env!("CARGO_PKG_NAME");
    println!(
        "{bin} {ver} — open a Flowstate thread on a project directory\n\
         \n\
         USAGE:\n    \
         {bin} [PATH]\n\
         \n\
         ARGS:\n    \
         PATH    Project directory (default: current directory)\n\
         \n\
         The Flowstate desktop app must be running. The CLI sends the\n\
         absolute path to the app, which adds it to the project list\n\
         (if missing) and opens a new thread using your saved defaults.",
        bin = bin,
        ver = env!("CARGO_PKG_VERSION"),
    );
}

/// Canonicalize `raw` to an absolute path, requiring that it
/// exists and is a directory. We canonicalize so the path that
/// hits the daemon matches whatever path it would store internally
/// — symlinks resolved, `.` / `..` collapsed, drive-letter casing
/// normalized on Windows. This makes the dedupe in the daemon's
/// `create_project` handler reliable.
fn resolve_dir(raw: &str) -> Result<PathBuf> {
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .context("read current directory")?
            .join(p)
    };
    let canonical = std::fs::canonicalize(&abs)
        .with_context(|| format!("path does not exist: {}", abs.display()))?;
    let meta = std::fs::metadata(&canonical)
        .with_context(|| format!("stat {}", canonical.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    Ok(canonical)
}

/// Resolve `<app_data_dir>/daemon.handshake` cross-platform. See
/// the module docs for why this matches Tauri's `app_data_dir()`.
fn handshake_file_path() -> Result<PathBuf> {
    let data = dirs::data_dir().ok_or_else(|| {
        anyhow!(
            "could not resolve OS data directory \
             (is $HOME / %APPDATA% set?)"
        )
    })?;
    Ok(data.join(BUNDLE_ID).join("daemon.handshake"))
}

fn read_handshake(path: &Path) -> Result<Handshake> {
    if !path.exists() {
        bail!(
            "Flowstate is not running. Launch the app and try again.\n\
             (no handshake file at {})",
            path.display()
        );
    }
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("read handshake file {}", path.display()))?;
    serde_json::from_str::<Handshake>(&body).with_context(|| {
        format!(
            "handshake file at {} is not valid JSON; \
             reinstall or relaunch Flowstate",
            path.display()
        )
    })
}

/// Cheap "is this process still alive?" probe. We don't need a
/// strict liveness signal — if the pid was reused since the
/// handshake was written, the worst case is a misleading early
/// success and a Connection refused on the POST below. Both paths
/// surface the same user-facing "not running" message.
///
/// `kill(pid, 0)` is the canonical Unix probe: returns 0 if the
/// process exists and we can signal it, -1 with `errno=ESRCH` if
/// it doesn't exist, `errno=EPERM` if it exists but we lack
/// permission to signal it (still alive for our purposes —
/// happens when running `flow` as a non-root user against a
/// flowstate started under a different uid, e.g. via `sudo`).
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: `kill(pid, 0)` is a side-effect-free permission probe.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // `std::io::Error::last_os_error()` reads errno via the
    // platform-correct shim (Rust libstd already abstracts the
    // `__errno_location` vs `__error` divergence).
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: `OpenProcess` returns a NULL handle on failure (which
    // includes "no such pid"). On success we close the handle right
    // away — we only opened it as a liveness probe.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

fn open_project(base_url: &str, path: &Path) -> Result<()> {
    let url = format!("{}/api/open-project", base_url.trim_end_matches('/'));
    let body = serde_json::json!({ "path": path.to_string_lossy() });
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_json(body);
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().unwrap_or_default();
            bail!(
                "Flowstate rejected the open-project request (HTTP {code}){}",
                if msg.is_empty() { String::new() } else { format!(": {msg}") }
            )
        }
        Err(ureq::Error::Transport(t)) => {
            bail!(
                "could not reach Flowstate at {base_url}: {t}\n\
                 (the app may have just exited; try relaunching it)"
            )
        }
    }
}
