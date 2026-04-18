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

    let version_output = Command::new(binary).args(version_args).output().await;
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
            };
        }
    };

    let version = first_non_empty_line(&version_output.stdout)
        .or_else(|| first_non_empty_line(&version_output.stderr));

    match Command::new(binary).args(auth_args).output().await {
        Ok(auth_output) => {
            let authenticated = auth_output.status.success();
            let message = if authenticated {
                Some(format!("{label} CLI is installed and authenticated."))
            } else {
                first_non_empty_line(&auth_output.stderr)
                    .or_else(|| first_non_empty_line(&auth_output.stdout))
                    .or_else(|| {
                        Some(match auth_hint {
                            Some(hint) => format!(
                                "{label} CLI is installed but not authenticated. {hint}"
                            ),
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
                }
            }
        }
    }
}
