// Read-side duplicate of daemon-core's ready file format. We duplicate
// rather than depend on daemon-core because daemon-client must stay a
// thin-client crate — the desktop shell doesn't need to link the entire
// runtime, providers, and SQLite stack just to read a JSON file.
//
// The two implementations must agree on:
//   1. Serde format of `ReadyFileContent` — trivially true because the
//      structs have identical field names and the daemon writes JSON.
//   2. File path resolution — uses the same `$TMPDIR/zenui/daemon-{hash}.json`
//      layout and the same `DefaultHasher`-based digest of the canonical
//      project root. Both sides run under the same Rust toolchain, so
//      DefaultHasher's deterministic seed produces identical output.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone)]
pub struct ReadyFile {
    path: PathBuf,
}

impl ReadyFile {
    pub fn for_project(project_root: &Path) -> Result<Self> {
        let canonical = fs::canonicalize(project_root)
            .with_context(|| format!("canonicalize {}", project_root.display()))?;
        let digest = short_hash(canonical.to_string_lossy().as_bytes());
        let dir = runtime_dir()?.join("zenui");
        fs::create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))?;
        Ok(Self {
            path: dir.join(format!("daemon-{digest}.json")),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
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

fn runtime_dir() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            return Ok(PathBuf::from(xdg));
        }
    }
    Ok(std::env::temp_dir())
}

fn short_hash(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn runtime_dir_public() -> Result<PathBuf> {
    runtime_dir()
}

pub(crate) fn short_hash_public(data: &[u8]) -> String {
    short_hash(data)
}
