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

/// One failed provisioning phase. Returned as part of
/// [`ProvisionOutcome`] so the Tauri shell can stash failures in
/// shared state and the Settings page can render per-phase Retry
/// banners after the splash dismisses.
#[derive(Debug, Clone, Serialize)]
pub struct ProvisionFailure {
    pub phase: ProvisionPhase,
    /// Full anyhow debug string (multi-line). Frontend usually shows
    /// only the first line in a banner and exposes the rest behind
    /// a "Show full error" disclosure.
    pub error: String,
}

/// Result of [`provision_runtimes`]. Always returned as `Ok(_)` —
/// individual phase failures populate `failures` instead of
/// short-circuiting the daemon boot. The caller decides whether to
/// surface the failures (we do, via Tauri state + the Settings UI).
#[derive(Debug, Clone, Serialize, Default)]
pub struct ProvisionOutcome {
    pub failures: Vec<ProvisionFailure>,
}

/// One of the runtime-provisioning phases the Tauri shell renders as
/// splash text during first launch. Kept as an enum rather than free
/// strings so the frontend can switch on `phase` without string-
/// matching against English copy that may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    /// Wire-format string used by the `provision` Tauri event and the
    /// `retry_provision_phase` command (matches `serde(rename_all =
    /// "kebab-case")`). Centralised so the Tauri shell parses incoming
    /// retry requests without re-spelling the variants.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::ClaudeSdk => "claude-sdk",
            Self::CopilotSdk => "copilot-sdk",
        }
    }

    /// Inverse of [`Self::as_str`]. Returns `None` for unknown strings
    /// so the Tauri command can return a typed error to the frontend.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "node" => Some(Self::Node),
            "claude-sdk" => Some(Self::ClaudeSdk),
            "copilot-sdk" => Some(Self::CopilotSdk),
            _ => None,
        }
    }
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
    Failed {
        phase: ProvisionPhase,
        error: String,
    },
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
pub fn provision_runtimes(reporter: &ProvisionReporter) -> ProvisionOutcome {
    let started_all = std::time::Instant::now();
    tracing::info!("provisioning bundled runtimes");

    let mut failures: Vec<ProvisionFailure> = Vec::new();

    // Each phase runs independently. A failure in one (e.g. Node
    // download blocked by a corporate proxy) no longer prevents the
    // others from being attempted, and the daemon still boots so the
    // user can recover via the Settings → Diagnostics retry buttons.
    //
    // Order is preserved (Node first) because the SDK bridges call
    // `ensure_available()` internally; if Node failed they will fail
    // too, which is fine — the Settings page will show two banners
    // and retrying Node first will unblock the rest.
    run_phase_collect(reporter, ProvisionPhase::Node, &mut failures, || {
        zenui_embedded_node::ensure_available().context("provision embedded Node.js")
    });
    run_phase_collect(reporter, ProvisionPhase::ClaudeSdk, &mut failures, || {
        zenui_provider_claude_sdk::ensure_bridge_available().context("provision Claude SDK bridge")
    });
    run_phase_collect(reporter, ProvisionPhase::CopilotSdk, &mut failures, || {
        zenui_provider_github_copilot::ensure_bridge_available().context("provision Copilot bridge")
    });

    let duration_ms = started_all.elapsed().as_millis() as u64;
    if failures.is_empty() {
        tracing::info!(duration_ms, "bundled runtimes provisioned");
    } else {
        tracing::warn!(
            duration_ms,
            failed = failures.len(),
            "bundled runtimes provisioned with failures; daemon will boot but affected providers are unavailable"
        );
    }
    // Always emit AllDone so the splash advances even if one or more
    // phases failed. The Failed events were emitted inline by
    // run_phase_collect; the splash decides whether to dismiss based
    // on the `welcome` message, not on this event.
    reporter(ProvisionEvent::AllDone { duration_ms });
    ProvisionOutcome { failures }
}

/// Re-run a single provisioning phase. Used by the Tauri command
/// `retry_provision_phase` so the user can recover from a transient
/// failure (no network on first launch, then Wi-Fi reconnects)
/// without restarting the app.
///
/// Emits the same `Started` / `Completed` / `Failed` events as the
/// initial pass so any open Settings UI listening to `provision`
/// updates live.
pub fn retry_phase(phase: ProvisionPhase, reporter: &ProvisionReporter) -> Result<()> {
    let mut failures: Vec<ProvisionFailure> = Vec::new();
    match phase {
        ProvisionPhase::Node => run_phase_collect(reporter, phase, &mut failures, || {
            zenui_embedded_node::ensure_available().context("provision embedded Node.js")
        }),
        ProvisionPhase::ClaudeSdk => run_phase_collect(reporter, phase, &mut failures, || {
            zenui_provider_claude_sdk::ensure_bridge_available()
                .context("provision Claude SDK bridge")
        }),
        ProvisionPhase::CopilotSdk => run_phase_collect(reporter, phase, &mut failures, || {
            zenui_provider_github_copilot::ensure_bridge_available()
                .context("provision Copilot bridge")
        }),
    }
    match failures.into_iter().next() {
        Some(f) => Err(anyhow::anyhow!(f.error)),
        None => Ok(()),
    }
}

fn run_phase_collect<F, T>(
    reporter: &ProvisionReporter,
    phase: ProvisionPhase,
    failures: &mut Vec<ProvisionFailure>,
    f: F,
) where
    F: FnOnce() -> Result<T>,
{
    reporter(ProvisionEvent::Started {
        phase,
        message: phase.message(),
    });
    let started = std::time::Instant::now();
    match f() {
        Ok(_) => {
            reporter(ProvisionEvent::Completed {
                phase,
                duration_ms: started.elapsed().as_millis() as u64,
            });
        }
        Err(e) => {
            let error = format!("{e:?}");
            // Emit the typed Failed event so the splash and any
            // open Settings UI can render the failure immediately.
            reporter(ProvisionEvent::Failed {
                phase,
                error: error.clone(),
            });
            failures.push(ProvisionFailure { phase, error });
        }
    }
}

/// Default reporter for call sites that don't want to surface events
/// anywhere (tests, headless contexts). Drops every event.
pub fn noop_reporter() -> Box<ProvisionReporter> {
    Box::new(|_| {})
}
