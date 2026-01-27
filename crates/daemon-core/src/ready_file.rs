use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const PROTOCOL_VERSION: u32 = 1;

/// Contents of the daemon's ready file. Written atomically after the local
/// server binds and begins accepting connections; deleted on graceful
/// shutdown. Clients discover a running daemon via this file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyFileContent {
    pub pid: u32,
    pub http_base: String,
    pub ws_url: String,
    pub protocol_version: u32,
    pub started_at: String,
    pub daemon_version: String,
    pub project_root: String,
}

impl ReadyFileContent {
    pub fn new(http_base: String, ws_url: String, project_root: String) -> Self {
        Self {
            pid: std::process::id(),
            http_base,
            ws_url,
            protocol_version: PROTOCOL_VERSION,
            started_at: chrono::Utc::now().to_rfc3339(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            project_root,
        }
    }
}

/// The daemon ready file is per-boot, per-user, per-project runtime state.
/// Lives in the OS runtime/temp dir so a stale file from a prior boot
/// never survives to confuse a fresh session.
#[derive(Debug, Clone)]
pub struct ReadyFile {
    path: PathBuf,
}

impl ReadyFile {
    /// Resolve the ready-file path for a given project root. The path is
    /// stable across invocations targeting the same project but distinct
    /// across different projects.
    pub fn for_project(project_root: &Path) -> Result<Self> {
        let canonical = fs::canonicalize(project_root).with_context(|| {
            format!("failed to canonicalize {}", project_root.display())
        })?;
        let digest = short_hash(canonical.to_string_lossy().as_bytes());
        let dir = runtime_dir()?.join("zenui");
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(Self {
            path: dir.join(format!("daemon-{digest}.json")),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Atomic write: write to a temp file, fsync, rename over the target.
    pub fn write_atomic(&self, content: &ReadyFileContent) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(content).context("serialize ready file")?;
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(&json).context("write ready file bytes")?;
            f.sync_all().context("fsync ready file")?;
        }
        fs::rename(&tmp, &self.path).with_context(|| {
            format!("rename {} -> {}", tmp.display(), self.path.display())
        })?;
        Ok(())
    }

    pub fn read(&self) -> Result<Option<ReadyFileContent>> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let content: ReadyFileContent =
                    serde_json::from_slice(&bytes).context("parse ready file")?;
                Ok(Some(content))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).context("read ready file"),
        }
    }

    pub fn delete(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).context("delete ready file"),
        }
    }
}

/// Per-boot runtime directory. macOS uses $TMPDIR (user-scoped), Linux uses
/// $XDG_RUNTIME_DIR with /tmp fallback, Windows uses %LOCALAPPDATA%\zenui.
fn runtime_dir() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            return Ok(PathBuf::from(xdg));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(dirs) = directories::ProjectDirs::from("com", "zenui", "zenui") {
            return Ok(dirs.data_local_dir().to_path_buf());
        }
    }
    // macOS and everything else: std::env::temp_dir() resolves $TMPDIR on
    // macOS (user-scoped) and /tmp on most other platforms.
    Ok(std::env::temp_dir())
}

fn short_hash(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_file_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let content = ReadyFileContent::new(
            "http://127.0.0.1:12345".to_string(),
            "ws://127.0.0.1:12345/ws".to_string(),
            tmp.path().to_string_lossy().into_owned(),
        );
        let rf = ReadyFile::for_project(tmp.path()).expect("ready file");
        rf.write_atomic(&content).expect("write");
        let read = rf.read().expect("read").expect("present");
        assert_eq!(read.http_base, content.http_base);
        assert_eq!(read.ws_url, content.ws_url);
        assert_eq!(read.protocol_version, PROTOCOL_VERSION);
        rf.delete().expect("delete");
        assert!(rf.read().expect("read-after-delete").is_none());
    }
}
