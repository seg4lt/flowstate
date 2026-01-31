// Read-side duplicate of daemon-core's ready file format. We duplicate
// rather than depend on daemon-core because daemon-client must stay a
// thin-client crate — the desktop shell doesn't need to link the entire
// runtime, providers, and SQLite stack just to read a JSON file.
//
// The two implementations must agree on:
//   1. Serde format of `ReadyFileContentV2` + `TransportAddressInfo` —
//      trivially true because both sides own identical serde definitions
//      and the daemon writes JSON.
//   2. File path resolution — uses the same `$TMPDIR/zenui/daemon-{hash}.json`
//      layout and the same `DefaultHasher`-based digest of the canonical
//      project root. Both sides run under the same Rust toolchain, so
//      DefaultHasher's deterministic seed produces identical output.
//
// This module ALSO accepts ready file v1 (the single-HTTP-transport
// format) for one release cycle, migrating v1 files into the v2 shape
// at read time. The shim is removed in the next release.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The v2 ready file format. Each daemon entry lists one or more
/// `TransportAddressInfo` entries so a multi-transport daemon can
/// advertise every wire it speaks in one file. Clients pick whichever
/// transport they support via `ClientConfig::preferred_transport`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyFileContentV2 {
    pub pid: u32,
    pub protocol_version: u32,
    pub started_at: String,
    pub daemon_version: String,
    pub project_root: String,
    pub transports: Vec<TransportAddressInfo>,
}

/// Legacy v1 ready file. Kept for one release cycle to let v0.2 clients
/// attach to a still-running v0.1 daemon. Migrated into `ReadyFileContentV2`
/// by synthesizing a single `{ kind: "http", ... }` transport entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReadyFileContentV1 {
    pub pid: u32,
    pub http_base: String,
    pub ws_url: String,
    pub protocol_version: u32,
    pub started_at: String,
    pub daemon_version: String,
    pub project_root: String,
}

impl ReadyFileContentV2 {
    fn from_v1(v1: ReadyFileContentV1) -> Self {
        Self {
            pid: v1.pid,
            protocol_version: 2,
            started_at: v1.started_at,
            daemon_version: v1.daemon_version,
            project_root: v1.project_root,
            transports: vec![TransportAddressInfo::Http {
                http_base: v1.http_base,
                ws_url: v1.ws_url,
            }],
        }
    }
}

/// Address info for one transport entry in the v2 ready file. Duplicated
/// from `daemon-core::transport::TransportAddressInfo` — keep them in
/// sync. The `#[serde(tag = "kind", rename_all = "kebab-case")]` matches
/// the daemon's serialization exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TransportAddressInfo {
    Http { http_base: String, ws_url: String },
    UnixSocket { path: String },
    NamedPipe { path: String },
    InProcess,
}

impl TransportAddressInfo {
    /// Short static string for the kind, matching the serde tag.
    pub fn kind(&self) -> &'static str {
        match self {
            TransportAddressInfo::Http { .. } => "http",
            TransportAddressInfo::UnixSocket { .. } => "unix-socket",
            TransportAddressInfo::NamedPipe { .. } => "named-pipe",
            TransportAddressInfo::InProcess => "in-process",
        }
    }
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

    /// Read the ready file and return it as `ReadyFileContentV2`. Accepts
    /// both v1 and v2 formats; v1 is migrated into the v2 shape internally
    /// so downstream code only handles v2.
    pub fn read(&self) -> Result<Option<ReadyFileContentV2>> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).context("read ready file"),
        };

        let raw: serde_json::Value =
            serde_json::from_slice(&bytes).context("parse ready file")?;
        let version = raw.get("protocol_version").and_then(|v| v.as_u64());
        match version {
            Some(2) => {
                let v2: ReadyFileContentV2 = serde_json::from_value(raw)
                    .context("parse ready file as v2")?;
                Ok(Some(v2))
            }
            Some(1) | None => {
                let v1: ReadyFileContentV1 = serde_json::from_value(raw)
                    .context("parse ready file as v1")?;
                Ok(Some(ReadyFileContentV2::from_v1(v1)))
            }
            Some(other) => bail!(
                "zenui-server ready file uses protocol_version {other}; \
                 this daemon-client supports [1, 2]. Please upgrade zenui."
            ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_migrates_to_v2_shape() {
        let v1_json = serde_json::json!({
            "pid": 12345,
            "http_base": "http://127.0.0.1:51000",
            "ws_url": "ws://127.0.0.1:51000/ws",
            "protocol_version": 1,
            "started_at": "2026-04-11T22:00:00Z",
            "daemon_version": "0.1.0",
            "project_root": "/tmp/test-project",
        });
        let v1: ReadyFileContentV1 = serde_json::from_value(v1_json).unwrap();
        let v2 = ReadyFileContentV2::from_v1(v1);
        assert_eq!(v2.pid, 12345);
        assert_eq!(v2.protocol_version, 2);
        assert_eq!(v2.transports.len(), 1);
        match &v2.transports[0] {
            TransportAddressInfo::Http { http_base, ws_url } => {
                assert_eq!(http_base, "http://127.0.0.1:51000");
                assert_eq!(ws_url, "ws://127.0.0.1:51000/ws");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn v2_roundtrip() {
        let v2 = ReadyFileContentV2 {
            pid: 99,
            protocol_version: 2,
            started_at: "2026-04-11T22:00:00Z".to_string(),
            daemon_version: "0.1.0".to_string(),
            project_root: "/tmp/p".to_string(),
            transports: vec![
                TransportAddressInfo::Http {
                    http_base: "http://127.0.0.1:50".to_string(),
                    ws_url: "ws://127.0.0.1:50/ws".to_string(),
                },
                TransportAddressInfo::UnixSocket {
                    path: "/tmp/zenui.sock".to_string(),
                },
            ],
        };
        let json = serde_json::to_string(&v2).unwrap();
        let back: ReadyFileContentV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(back.transports.len(), 2);
        assert_eq!(back.transports[0].kind(), "http");
        assert_eq!(back.transports[1].kind(), "unix-socket");
    }

    #[test]
    fn read_dispatches_on_protocol_version() {
        // Hand-crafted v1 string parses.
        let v1_str = r#"{
            "pid": 1,
            "http_base": "http://127.0.0.1:1",
            "ws_url": "ws://127.0.0.1:1/ws",
            "protocol_version": 1,
            "started_at": "x",
            "daemon_version": "y",
            "project_root": "/z"
        }"#;
        let raw: serde_json::Value = serde_json::from_str(v1_str).unwrap();
        let version = raw.get("protocol_version").and_then(|v| v.as_u64());
        assert_eq!(version, Some(1));

        // Unknown version errors cleanly.
        let v99_str = r#"{"protocol_version": 99}"#;
        let raw: serde_json::Value = serde_json::from_str(v99_str).unwrap();
        let version = raw.get("protocol_version").and_then(|v| v.as_u64());
        assert_eq!(version, Some(99));
    }
}
