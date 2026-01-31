//! ZenUI daemon client.
//!
//! A small, dependency-lean crate every frontend (tao-web-shell, a future
//! Tauri shell, a future CLI, a future GPUI shell) uses to locate — or
//! auto-spawn — a running `zenui-server` and attach to whichever transport
//! it speaks. Deliberately does *not* depend on `daemon-core` or on any
//! transport crate, so building a shell does not drag in the entire
//! runtime + provider + SQLite stack.
//!
//! # Ready file format
//!
//! `daemon-client` reads ready file v2 (a list of `TransportAddressInfo`
//! entries) and transparently accepts v1 files (single HTTP entry) via an
//! internal migration. Both formats coexist for one release cycle; v1
//! support is scheduled for removal in the next release.
//!
//! # Transport preference
//!
//! The caller tells `connect_or_spawn` which transport it speaks via
//! `ClientConfig::preferred_transport`. `connect_or_spawn` filters the
//! ready file's transport list accordingly. If no match exists, the
//! caller gets a clear error pointing at a daemon restart.

mod ready_file;
mod spawn;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub use ready_file::{ReadyFile, ReadyFileContentV2, TransportAddressInfo};

/// Which transport the caller wants. `Any` picks the first entry the
/// daemon offers in ready-file order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPreference {
    Any,
    Http,
    UnixSocket,
    NamedPipe,
    InProcess,
}

impl TransportPreference {
    fn matches(&self, info: &TransportAddressInfo) -> bool {
        match (self, info) {
            (TransportPreference::Any, _) => true,
            (TransportPreference::Http, TransportAddressInfo::Http { .. }) => true,
            (TransportPreference::UnixSocket, TransportAddressInfo::UnixSocket { .. }) => true,
            (TransportPreference::NamedPipe, TransportAddressInfo::NamedPipe { .. }) => true,
            (TransportPreference::InProcess, TransportAddressInfo::InProcess) => true,
            _ => false,
        }
    }
}

/// What a connected client needs to talk to the daemon. The `address`
/// field tells the caller which wire the daemon offered them; use
/// `as_http()` as a convenience when the caller specifically wants HTTP
/// endpoints.
#[derive(Debug, Clone)]
pub struct DaemonHandle {
    pub pid: u32,
    pub address: TransportAddressInfo,
}

impl DaemonHandle {
    /// Returns `(http_base, ws_url)` when `self.address` is HTTP; `None`
    /// for any other transport. Lets the tao-web-shell stay compact.
    pub fn as_http(&self) -> Option<HttpEndpoints<'_>> {
        match &self.address {
            TransportAddressInfo::Http { http_base, ws_url } => Some(HttpEndpoints {
                http_base,
                ws_url,
            }),
            _ => None,
        }
    }
}

/// Borrowed view of the HTTP endpoint strings inside a `DaemonHandle::Http`.
/// Cheap to construct — does not allocate. Clone the strings before
/// dropping the parent `DaemonHandle` if you need them beyond its
/// lifetime.
#[derive(Debug, Clone, Copy)]
pub struct HttpEndpoints<'a> {
    pub http_base: &'a str,
    pub ws_url: &'a str,
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Canonical project root. The ready file and spawn lock are keyed by
    /// a hash of this path, so different projects run independent daemons.
    pub project_root: PathBuf,
    /// Override for `zenui-server` binary discovery (tests and dev).
    pub server_binary: Option<PathBuf>,
    /// Total time budget for waiting on the ready file after spawning.
    pub spawn_timeout: Duration,
    /// Per-call timeout for `/api/health` probes.
    pub health_timeout: Duration,
    /// Transport the caller wants to use. Defaults to `Http` because
    /// that's the only transport shipped today.
    pub preferred_transport: TransportPreference,
}

impl ClientConfig {
    /// Build a config rooted at the current working directory.
    pub fn for_current_project() -> Result<Self> {
        let cwd = std::env::current_dir().context("resolve current working directory")?;
        let project_root = std::fs::canonicalize(&cwd)
            .with_context(|| format!("canonicalize {}", cwd.display()))?;
        Ok(Self {
            project_root,
            server_binary: None,
            spawn_timeout: Duration::from_secs(10),
            health_timeout: Duration::from_millis(500),
            preferred_transport: TransportPreference::Http,
        })
    }
}

/// Connect to a running daemon or spawn one and wait for it to become
/// ready. Race-safe against concurrent callers: an fs4 advisory lock
/// serializes the spawn attempt, and the ready file is re-read after the
/// lock is acquired so a caller that lost the race returns cleanly.
pub fn connect_or_spawn(config: &ClientConfig) -> Result<DaemonHandle> {
    let ready = ReadyFile::for_project(&config.project_root)?;

    // Happy path: existing ready file + a matching transport + health OK.
    if let Some(content) = ready.read()? {
        if let Some(handle) = try_attach(&content, config)? {
            return Ok(handle);
        }
        // Stale file — daemon died without cleaning up. Remove so the
        // spawn branch below doesn't trip over it.
        let _ = ready.delete();
    }

    // Take the advisory lock so only one client spawns at a time.
    let lock = spawn::acquire_spawn_lock(&config.project_root)
        .context("acquire zenui-server spawn lock")?;

    // Re-read under the lock — another client may have won the race.
    if let Some(content) = ready.read()? {
        if let Some(handle) = try_attach(&content, config)? {
            drop(lock);
            return Ok(handle);
        }
        let _ = ready.delete();
    }

    // We own the spawn. Kick off zenui-server; its own `start` subcommand
    // handles detachment and polls its ready file before returning.
    spawn::spawn_daemon(config)?;
    drop(lock);

    // Poll until the ready file appears AND a matching transport is healthy.
    let deadline = Instant::now() + config.spawn_timeout;
    loop {
        if let Some(content) = ready.read()? {
            if let Some(handle) = try_attach(&content, config)? {
                return Ok(handle);
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for zenui-server to become ready for project {} \
                 (transport preference: {:?})",
                config.project_root.display(),
                config.preferred_transport
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Attempt to find a matching transport in a ready-file payload, probe
/// its liveness, and build a `DaemonHandle`. Returns `Ok(None)` when the
/// ready file doesn't offer a matching transport OR when the transport
/// fails its health check (in which case the caller should treat the
/// ready file as stale).
fn try_attach(
    content: &ReadyFileContentV2,
    config: &ClientConfig,
) -> Result<Option<DaemonHandle>> {
    // Find first transport that matches the caller's preference.
    let Some(address) = content
        .transports
        .iter()
        .find(|t| config.preferred_transport.matches(t))
        .cloned()
    else {
        // No transport matches. This could be "daemon exists but offers
        // only a wire we don't speak" — which is a permanent condition,
        // not a stale-file situation. We bubble it as an error instead
        // of returning None because we don't want the caller to spawn a
        // new daemon on top of the existing one.
        let kinds: Vec<&'static str> =
            content.transports.iter().map(|t| t.kind()).collect();
        bail!(
            "zenui-server is running for this project but offers {:?}; \
             client wants {:?}. Stop the daemon (`zenui-server stop`) and \
             restart it with a transport list that includes your preferred wire.",
            kinds,
            config.preferred_transport
        );
    };

    // Liveness probe. Only HTTP is implemented today — other transports
    // return Ok(()) unconditionally as a placeholder until their own
    // health-probe mechanisms land.
    if !probe_live(&address, config.health_timeout) {
        return Ok(None);
    }

    Ok(Some(DaemonHandle {
        pid: content.pid,
        address,
    }))
}

/// Dispatch a liveness probe to the appropriate transport. Returns `true`
/// if the probe succeeds. Used to detect stale ready files.
fn probe_live(address: &TransportAddressInfo, timeout: Duration) -> bool {
    match address {
        TransportAddressInfo::Http { http_base, .. } => http_health_check(http_base, timeout).is_ok(),
        // Non-HTTP transports don't yet have defined liveness probes;
        // assume live until their transports implement something.
        TransportAddressInfo::UnixSocket { .. }
        | TransportAddressInfo::NamedPipe { .. }
        | TransportAddressInfo::InProcess => true,
    }
}

fn http_health_check(http_base: &str, timeout: Duration) -> Result<()> {
    let url = format!("{}/api/health", http_base);
    ureq::get(&url)
        .timeout(timeout)
        .call()
        .with_context(|| format!("GET {} failed", url))?;
    Ok(())
}
