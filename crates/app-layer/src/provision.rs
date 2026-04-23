//! Runtime provisioning — Node.js + provider-SDK `node_modules` —
//! done once at app startup so the first user-initiated turn isn't
//! the one paying for the 30–90 second first-launch install.
//!
//! Called from the Tauri shell's setup closure
//! (`apps/flowstate/src-tauri/src/lib.rs`) on a `spawn_blocking`
//! thread; the caller wires [`ProvisionReporter`] to
//! `app.emit("provision", event)` so the webview's
//! `<ProvisioningSplash />` can render progress.
//!
//! Two sources of bytes depending on how the binary was built:
//! - Default — `embedded-node` downloads Node from nodejs.org; the
//!   provider bridges `npm install --omit=dev` their
//!   `node_modules/` from npmjs.org on first launch.
//! - `--features embed-all` — Node tarball and the full bridge
//!   `node_modules/` trees are baked in; every phase below is a
//!   sub-millisecond sentinel check and the splash never paints.

use anyhow::{Context, Result};
use serde::Serialize;

/// One of the runtime-provisioning phases the Tauri shell renders as
/// splash text during first launch. Kept as an enum rather than free
/// strings so the frontend can switch on `phase` without string-
/// matching against English copy that may change.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvisionPhase {
    /// Downloading + extracting the Node.js runtime from nodejs.org.
    Node,
    /// Hydrating `@anthropic-ai/claude-agent-sdk` node_modules via
    /// `npm install` against npmjs.org.
    ClaudeSdk,
    /// Hydrating `@github/copilot-sdk` node_modules via npm.
    CopilotSdk,
}

impl ProvisionPhase {
    /// Short human-readable description the splash screen renders
    /// directly. "Installing …" rather than "Provisioning …" because
    /// that's what users actually see happening (a multi-MB download).
    pub fn message(self) -> &'static str {
        match self {
            Self::Node => "Downloading Node.js runtime",
            Self::ClaudeSdk => "Installing Claude SDK (first launch only)",
            Self::CopilotSdk => "Installing GitHub Copilot SDK (first launch only)",
        }
    }
}

/// Progress events emitted by [`provision_runtimes`] via the
/// `reporter` callback. The Tauri shell serializes these to the
/// webview as `provision` events; the splash screen consumes them to
/// swap phase labels.
///
/// Serialized with `#[serde(tag = "kind")]` → each variant shows up
/// as `{ kind: "started", phase: "node", message: "…" }` etc. on the
/// wire — matches how the React listener destructures them.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProvisionEvent {
    /// A phase has started; splash should display `message`.
    Started {
        phase: ProvisionPhase,
        message: &'static str,
    },
    /// A phase completed. `duration_ms` is wall-clock; useful for
    /// telemetry, ignored by the splash.
    Completed {
        phase: ProvisionPhase,
        duration_ms: u64,
    },
    /// Every phase finished successfully. Splash keeps rendering
    /// until the app's own `ready` flag flips via the `welcome`
    /// message, so this event is informational — mostly telemetry.
    AllDone { duration_ms: u64 },
    /// A phase failed. Splash renders the error until the daemon
    /// either retries or the user restarts. `error` is the full
    /// anyhow debug string; the React side decides what to show.
    Failed { phase: ProvisionPhase, error: String },
}

/// Callback type the caller supplies to receive provisioning progress.
/// `Box<dyn Fn>` rather than a generic because the callback is stored
/// behind an `Arc` in the Tauri setup closure — keeps the call sites
/// simple and there's no hot path where monomorphization would matter.
pub type ProvisionReporter = dyn Fn(ProvisionEvent) + Send + Sync;

/// Eagerly extract (or download, in non-embed builds) the Node.js
/// runtime and every provider bridge that needs one, so the app is
/// fully ready to spawn adapters the moment `bootstrap_core_async`
/// finishes.
///
/// Why eager (instead of lazy on first provider call):
/// - **Predictable UX** — a laggy first turn because Node.js is
///   downloading in the background is worse than a slightly longer
///   startup where the user sees "setting up" once.
/// - **Fail fast** — network errors surface before the webview flips
///   to "ready", so the user sees a clear error on the splash rather
///   than a mystery-broken provider five minutes in.
///
/// `reporter` receives one event per phase transition (`Started`,
/// `Completed`, `Failed`, `AllDone`). Pass [`noop_reporter`] when no
/// UI surfacing is needed (tests, headless contexts).
///
/// Runs synchronously (blocking IO) — the caller bridges via
/// `spawn_blocking`.
pub fn provision_runtimes(reporter: &ProvisionReporter) -> Result<()> {
    let started_all = std::time::Instant::now();
    tracing::info!("provisioning bundled runtimes");

    // Node.js runtime. Shared by every SDK-style provider; do it first
    // so a failure here short-circuits bridge extraction that would
    // also need Node.
    run_phase(reporter, ProvisionPhase::Node, || {
        zenui_embedded_node::ensure_available().context("provision embedded Node.js")
    })?;

    // Provider bridges. Each returns a cached `BridgeRuntime` struct
    // we discard here — the point of this call is the side effect of
    // populating the cache directory (and running `npm install` in
    // download mode) before any adapter reaches for it.
    run_phase(reporter, ProvisionPhase::ClaudeSdk, || {
        zenui_provider_claude_sdk::ensure_bridge_available()
            .context("provision Claude SDK bridge")
    })?;
    run_phase(reporter, ProvisionPhase::CopilotSdk, || {
        zenui_provider_github_copilot::ensure_bridge_available()
            .context("provision Copilot bridge")
    })?;

    let duration_ms = started_all.elapsed().as_millis() as u64;
    tracing::info!(duration_ms, "bundled runtimes provisioned");
    reporter(ProvisionEvent::AllDone { duration_ms });
    Ok(())
}

fn run_phase<F, T>(reporter: &ProvisionReporter, phase: ProvisionPhase, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    reporter(ProvisionEvent::Started {
        phase,
        message: phase.message(),
    });
    let started = std::time::Instant::now();
    match f() {
        Ok(v) => {
            reporter(ProvisionEvent::Completed {
                phase,
                duration_ms: started.elapsed().as_millis() as u64,
            });
            Ok(v)
        }
        Err(e) => {
            // Emit the typed Failed event BEFORE returning so a splash
            // can show the failure context; the caller still sees the
            // original anyhow error.
            reporter(ProvisionEvent::Failed {
                phase,
                error: format!("{e:?}"),
            });
            Err(e)
        }
    }
}

/// Default reporter for call sites that don't want to surface events
/// anywhere (tests, headless contexts). Drops every event.
pub fn noop_reporter() -> Box<ProvisionReporter> {
    Box::new(|_| {})
}
