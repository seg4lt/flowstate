//! Transport-composition contract.
//!
//! A `Transport` is a wire adapter that exposes `RuntimeCore` to
//! out-of-process clients (HTTP, Unix socket, Tauri IPC, wry custom
//! protocol — whatever). `daemon-core::run_blocking` accepts a
//! `Vec<Box<dyn Transport>>` and drives each one's lifecycle alongside
//! the shared runtime and idle watchdog.
//!
//! This module lives in `runtime-core` (rather than `daemon-core`) so
//! transport crates can depend on it without pulling in `daemon-core`.
//! That inversion is what lets `daemon-core` take *optional* feature-
//! gated dependencies on concrete transport crates and re-export them,
//! without introducing a dependency cycle.
//!
//! # Two-stage lifecycle
//!
//! Starting a transport is split into two synchronous stages plus an
//! async shutdown, rather than a single `async fn start()`, for one
//! specific reason: **OS-resource bind errors must surface synchronously
//! on the host thread, before any tokio runtime is running.** If `start()`
//! were async, a port-in-use or permission error would be buried inside a
//! spawned task and the daemon would have no clean way to fail fast.
//!
//! 1. **Construction** happens in the app's `main()` when building the
//!    `Vec<Box<dyn Transport>>`. Transports hold only configuration
//!    (bind address, socket path, ...); no OS resources are claimed.
//! 2. **`Transport::bind()`** — called on the host thread by
//!    `daemon-core::run_blocking` before entering `tokio_runtime.block_on`.
//!    This is the one place a transport is allowed to claim OS resources
//!    (`StdTcpListener::bind`, `UnixListener::bind`, ...). It must NOT
//!    touch any tokio API — tokio isn't running yet. Returns a
//!    `Box<dyn Bound>` that owns the claimed resource.
//! 3. **`Bound::serve()`** — called inside `tokio_runtime.block_on`, once
//!    the runtime is live and `Arc<RuntimeCore>` is ready. This is where
//!    the transport converts its `StdTcpListener` (or equivalent) into a
//!    tokio-aware one and spawns its accept loop via `tokio::spawn`.
//!    Returns a `Box<dyn TransportHandle>`.
//! 4. **`TransportHandle::shutdown()`** — async. Called during graceful
//!    shutdown to drain existing connections within the caller's grace
//!    budget.
//!
//! # Writing a transport
//!
//! A third-party transport crate implements the three traits below and
//! exposes its `Transport` struct publicly. The app binary depends on
//! that crate and constructs the transport in its `Vec<Box<dyn Transport>>`.
//! No modifications to `daemon-core` are required.
//!
//! See `crates/middleman/transport-http/src/lib.rs` for the reference
//! implementation.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{ConnectionObserver, RuntimeCore};

/// A configured transport that has not yet claimed any OS resources.
/// Constructed by the app in its `main()` function.
pub trait Transport: Send {
    /// Short static identifier used in ready-file entries, log output,
    /// and client-side transport preference matching. Convention:
    /// lowercase, hyphenated — `"http"`, `"unix-socket"`, `"wry-ipc"`.
    /// Third parties should prefix to avoid collisions — `"acme-grpc"`.
    fn kind(&self) -> &'static str;

    /// Claim OS resources synchronously on the host thread. This is the
    /// only place port-in-use / permission / file-conflict errors can
    /// surface during daemon startup. **Must NOT call any tokio API** —
    /// tokio isn't running yet.
    ///
    /// Returns a `Box<dyn Bound>` that owns the claimed resource. The
    /// `Bound` is passed into `serve()` inside the tokio runtime once it
    /// starts.
    fn bind(self: Box<Self>) -> Result<Box<dyn Bound>>;
}

/// A transport that has claimed its OS resource and is waiting for the
/// daemon's tokio runtime to become live before starting its accept loop.
pub trait Bound: Send {
    /// Same identifier as the source `Transport::kind()`.
    fn kind(&self) -> &'static str;

    /// Report the address this transport is listening on. Used to
    /// populate the ready file before clients can connect.
    fn address_info(&self) -> TransportAddressInfo;

    /// Start serving. Must be called inside the daemon's tokio runtime
    /// context (i.e. from inside `runtime.block_on(...)` or a task
    /// spawned on it). Consumes `self`, so calling twice is a compile
    /// error.
    ///
    /// On success, returns a `TransportHandle` that owns the spawned
    /// accept-loop task and a shutdown signal. The handle's `shutdown()`
    /// must be awaited during graceful shutdown to drain connections.
    fn serve(
        self: Box<Self>,
        runtime: Arc<RuntimeCore>,
        observer: Arc<dyn ConnectionObserver>,
    ) -> Result<Box<dyn TransportHandle>>;
}

/// A running transport. Returned by `Bound::serve()`. Owns the accept
/// loop's `JoinHandle` and a oneshot shutdown signal; dropping it without
/// `shutdown().await` aborts the accept loop without draining in-flight
/// connections — always prefer `shutdown().await` in graceful paths.
#[async_trait]
pub trait TransportHandle: Send + Sync {
    /// Same identifier as the source `Transport::kind()`.
    fn kind(&self) -> &'static str;

    /// Address this transport is reachable on. Used by `run_blocking`
    /// to build the ready file after every transport is serving.
    fn address_info(&self) -> TransportAddressInfo;

    /// Stop accepting new connections, drain existing ones, release the
    /// OS resource. Must complete within the caller's shutdown-grace
    /// budget — implementations should not block indefinitely.
    async fn shutdown(self: Box<Self>);
}

/// Address information a transport reports to `run_blocking`. Serialized
/// into the daemon's ready file under the `transports[]` array so clients
/// can discover where to connect.
///
/// The `#[serde(tag = "kind", rename_all = "kebab-case")]` means each
/// variant serializes as `{"kind": "http", ...}` and unknown `kind`
/// values fail deserialization cleanly on the client side.
///
/// `Custom` is intentionally NOT included in this release. The closed
/// enum keeps serde behavior predictable and avoids `#[serde(untagged)]`
/// ordering pitfalls. When the first real third-party transport lands,
/// we can either add its variant upstream or introduce a `Custom` arm
/// at that time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TransportAddressInfo {
    Http { http_base: String, ws_url: String },
    UnixSocket { path: String },
    NamedPipe { path: String },
    InProcess,
}

impl TransportAddressInfo {
    /// Short static string for the kind of address this is, matching the
    /// serde tag. Useful for filtering in `daemon-client` when a caller
    /// specifies a `TransportPreference`.
    pub fn kind(&self) -> &'static str {
        match self {
            TransportAddressInfo::Http { .. } => "http",
            TransportAddressInfo::UnixSocket { .. } => "unix-socket",
            TransportAddressInfo::NamedPipe { .. } => "named-pipe",
            TransportAddressInfo::InProcess => "in-process",
        }
    }
}
