use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;

use crate::ClientConfig;
use crate::ready_file::{runtime_dir_public, short_hash_public};

/// Holds an OS advisory lock on the spawn lock file. Drops the file (and
/// thus releases the lock) when the guard goes out of scope.
pub struct SpawnLock {
    _file: std::fs::File,
}

/// Acquire an exclusive advisory lock on the per-project spawn lock file.
/// Retries with a short backoff and fails after ~2 seconds. Caller must
/// keep the returned `SpawnLock` alive for as long as the critical section
/// needs to be held.
pub fn acquire_spawn_lock(project_root: &Path) -> Result<SpawnLock> {
    let lock_path = spawn_lock_path(project_root)?;
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => return Ok(SpawnLock { _file: file }),
            Ok(false) => {
                if Instant::now() >= deadline {
                    bail!(
                        "failed to acquire spawn lock at {} within 2 seconds",
                        lock_path.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                return Err(err).context(format!(
                    "advisory lock on {} failed",
                    lock_path.display()
                ));
            }
        }
    }
}

/// Invoke `zenui-server start --project-root=<root>` synchronously. The
/// server's own `start` subcommand handles the fork-exec and polls its
/// own ready file before returning — so by the time this call returns,
/// the daemon is up (or the spawn has reported an error).
pub fn spawn_daemon(config: &ClientConfig) -> Result<()> {
    let server_bin = resolve_server_binary(config)?;
    let status = Command::new(&server_bin)
        .arg("start")
        .arg("--project-root")
        .arg(&config.project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("invoke {}", server_bin.display()))?;
    if !status.success() {
        bail!("zenui-server start exited with {status}");
    }
    Ok(())
}

/// Resolve the `zenui-server` binary in priority order:
/// 1. `config.server_binary` (explicit override — tests and dev)
/// 2. `$ZENUI_SERVER_BIN` environment variable
/// 3. A sibling of `current_exe()` in the same directory (the normal
///    case when tao-web-shell and daemon-bin are built into the same
///    `target/debug/` or installed into the same prefix).
fn resolve_server_binary(config: &ClientConfig) -> Result<PathBuf> {
    if let Some(path) = config.server_binary.as_ref() {
        if path.exists() {
            return Ok(path.clone());
        }
    }
    if let Ok(path) = std::env::var("ZENUI_SERVER_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            let candidate = parent.join(server_binary_name());
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "could not locate zenui-server binary; set ZENUI_SERVER_BIN or place it next to the shell binary"
    )
}

#[cfg(target_os = "windows")]
fn server_binary_name() -> &'static str {
    "zenui-server.exe"
}

#[cfg(not(target_os = "windows"))]
fn server_binary_name() -> &'static str {
    "zenui-server"
}

fn spawn_lock_path(project_root: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(project_root)
        .with_context(|| format!("canonicalize {}", project_root.display()))?;
    let digest = short_hash_public(canonical.to_string_lossy().as_bytes());
    let dir = runtime_dir_public()?.join("zenui");
    Ok(dir.join(format!("spawn-{digest}.lock")))
}
