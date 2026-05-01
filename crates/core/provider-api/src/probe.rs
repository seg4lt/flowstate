//! Shared CLI health-probe helper used by adapters that wrap a
//! command-line binary (Codex, Claude CLI, and future CLI-backed
//! providers).
//!
//! Adapters that speak JSON-RPC over stdio (e.g. `provider-github-copilot-cli`)
//! need a different flow — this helper is only for the classic
//! `--version` / `auth status` shape.

use tokio::process::Command;

use crate::{
    ProviderFeatures, ProviderKind, ProviderModel, ProviderStatus, ProviderStatusLevel,
    helpers::first_non_empty_line,
};

/// Per-provider knobs passed to [`probe_cli`]. Keeps the call site
/// readable when a provider needs custom messaging.
pub struct ProbeCliOptions<'a> {
    /// Which provider is being probed (used for the `kind` / `label`
    /// fields on the returned `ProviderStatus`).
    pub kind: ProviderKind,
    /// Path or name of the binary to probe. Typically resolved via
    /// [`crate::find_cli_binary`] first.
    pub binary: &'a str,
    /// Argv passed to fetch the version string. Both stdout and stderr
    /// are scanned for the first non-empty line.
    pub version_args: &'a [&'a str],
    /// Argv passed to check auth. Exit status 0 means authenticated.
    pub auth_args: &'a [&'a str],
    /// Model catalog to report on the returned `ProviderStatus`.
    pub models: Vec<ProviderModel>,
    /// Feature flags to report on the returned `ProviderStatus`.
    pub features: ProviderFeatures,
    /// Shown when the binary can't be invoked at all. `{error}` is
    /// substituted with the underlying OS error. If `None`, a generic
    /// "is unavailable" message is used.
    pub install_hint: Option<&'a str>,
    /// Shown when the binary runs but auth fails. If `None`, a generic
    /// "installed but not authenticated" message is used.
    pub auth_hint: Option<&'a str>,
    /// Treat `auth_args` launch failure (as opposed to exit-nonzero) as
    /// success. Useful for CLIs where the auth subcommand may not exist
    /// on older versions — the user can still try to run a turn.
    pub auth_err_is_ok: bool,
}

/// Probe a CLI-based provider's health: installed? authenticated? what
/// version? Returns a fully-populated `ProviderStatus` ready to send to
/// the client.
pub async fn probe_cli(options: ProbeCliOptions<'_>) -> ProviderStatus {
    let ProbeCliOptions {
        kind,
        binary,
        version_args,
        auth_args,
        models,
        features,
        install_hint,
        auth_hint,
        auth_err_is_ok,
    } = options;
    let label = kind.label().to_string();

    // Resolve to an absolute path through the workspace resolver so
    // the OS launcher uses the user's configured extras + platform
    // fallbacks (Windows `CreateProcessW` ignores any PATH env we
    // set on the child for module resolution). Most callers already
    // pass an absolute path here — `resolve_cli_command` is a fast
    // pass-through in that case, since `find_cli_binary` short-
    // circuits on a hit in the first PATH walk.
    let resolved_binary = crate::resolve_cli_command(binary);
    let mut version_cmd = Command::new(&resolved_binary);
    crate::hide_console_window_tokio(&mut version_cmd);
    // Augment the child's PATH so any subprocess the probe forks
    // sees the user's configured extras too. Critical on Windows
    // where GUI launches inherit a stripped PATH and the user has
    // pointed flowstate at e.g. C:\Users\foo\.local\bin.
    version_cmd.env("PATH", crate::path_with_extras(&[]));
    let version_output = version_cmd.args(version_args).output().await;
    let version_output = match version_output {
        Ok(out) => out,
        Err(error) => {
            let message = match install_hint {
                Some(hint) => format!("{label} CLI is unavailable: {error}. {hint}"),
                None => format!("{label} CLI is unavailable: {error}"),
            };
            return ProviderStatus {
                kind,
                label,
                installed: false,
                authenticated: false,
                version: None,
                status: ProviderStatusLevel::Error,
                message: Some(message),
                models,
                enabled: true,
                features,
                update_available: false,
                latest_version: None,
            };
        }
    };

    let version = first_non_empty_line(&version_output.stdout)
        .or_else(|| first_non_empty_line(&version_output.stderr));

    let mut auth_cmd = Command::new(&resolved_binary);
    crate::hide_console_window_tokio(&mut auth_cmd);
    auth_cmd.env("PATH", crate::path_with_extras(&[]));
    match auth_cmd.args(auth_args).output().await {
        Ok(auth_output) => {
            let authenticated = auth_output.status.success();
            let message = if authenticated {
                Some(format!("{label} CLI is installed and authenticated."))
            } else {
                first_non_empty_line(&auth_output.stderr)
                    .or_else(|| first_non_empty_line(&auth_output.stdout))
                    .or_else(|| {
                        Some(match auth_hint {
                            Some(hint) => {
                                format!("{label} CLI is installed but not authenticated. {hint}")
                            }
                            None => format!("{label} CLI is installed but not authenticated."),
                        })
                    })
            };

            ProviderStatus {
                kind,
                label,
                installed: true,
                authenticated,
                version,
                status: if authenticated {
                    ProviderStatusLevel::Ready
                } else {
                    ProviderStatusLevel::Warning
                },
                message,
                models,
                enabled: true,
                features,
                update_available: false,
                latest_version: None,
            }
        }
        Err(error) => {
            if auth_err_is_ok {
                ProviderStatus {
                    kind,
                    label: label.clone(),
                    installed: true,
                    authenticated: true,
                    version,
                    status: ProviderStatusLevel::Ready,
                    message: Some(format!(
                        "{label} CLI is installed (auth subcommand unavailable; assuming authenticated)."
                    )),
                    models,
                    enabled: true,
                    features,
                    update_available: false,
                    latest_version: None,
                }
            } else {
                ProviderStatus {
                    kind,
                    label: label.clone(),
                    installed: true,
                    authenticated: false,
                    version,
                    status: ProviderStatusLevel::Warning,
                    message: Some(format!(
                        "{label} CLI is installed, but auth probing failed: {error}"
                    )),
                    models,
                    enabled: true,
                    features,
                    update_available: false,
                    latest_version: None,
                }
            }
        }
    }
}

/// Result of running a CLI's own update-check command.
///
/// Adapters call [`probe_update_check`] (or roll their own equivalent)
/// after the main `probe_cli` and overwrite `update_available` /
/// `latest_version` on the returned [`ProviderStatus`] when the probe
/// signals a newer release exists. Adapters whose CLI has no update
/// probe leave the defaults (`false` / `None`).
#[derive(Debug, Clone, Default)]
pub struct UpdateCheckOutcome {
    pub update_available: bool,
    pub latest_version: Option<String>,
}

/// Lightweight wrapper for the "run a CLI subcommand and look at the
/// output for an update marker" pattern. Returns whatever bytes the
/// command produced on stdout/stderr; the caller decides how to parse
/// them since each CLI's update line format is different.
///
/// Returns `None` when the command can't be launched at all (binary
/// missing, permission denied) — adapters treat that as "no update
/// info" rather than "no update available", since the failure mode
/// is the same as a CLI without an update probe.
pub async fn probe_update_check(
    binary: &str,
    args: &[&str],
) -> Option<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut cmd = Command::new(crate::resolve_cli_command(binary));
    crate::hide_console_window_tokio(&mut cmd);
    cmd.env("PATH", crate::path_with_extras(&[]));
    cmd.args(args)
        .output()
        .await
        .ok()
        .map(|out| (out.status, out.stdout, out.stderr))
}
