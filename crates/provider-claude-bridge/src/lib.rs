use async_trait::async_trait;
use tokio::process::Command;
use zenui_provider_api::{
    ProviderAdapter, ProviderKind, ProviderStatus, ProviderStatusLevel, ProviderTurnOutput,
    SessionDetail,
};

#[derive(Debug, Clone)]
pub struct ClaudeBridgeAdapter {
    binary_path: String,
    working_directory: std::path::PathBuf,
}

impl ClaudeBridgeAdapter {
    pub fn new(working_directory: std::path::PathBuf) -> Self {
        Self {
            binary_path: "claude".to_string(),
            working_directory,
        }
    }
}

#[async_trait]
impl ProviderAdapter for ClaudeBridgeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    async fn health(&self) -> ProviderStatus {
        probe_cli(
            &self.binary_path,
            ProviderKind::Claude,
            &["--version"],
            &["auth", "status"],
        )
        .await
    }

    async fn start_session(
        &self,
        _session: &SessionDetail,
    ) -> Result<Option<zenui_provider_api::ProviderSessionState>, String> {
        Ok(None)
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &str,
    ) -> Result<ProviderTurnOutput, String> {
        let prompt = session.format_turn_context(input);
        let output = Command::new(&self.binary_path)
            .arg("-p")
            .arg("--add-dir")
            .arg(&self.working_directory)
            .arg("--permission-mode")
            .arg("acceptEdits")
            .arg("--output-format")
            .arg("text")
            .arg(&prompt)
            .current_dir(&self.working_directory)
            .output()
            .await
            .map_err(|error| format!("failed to launch Claude CLI: {error}"))?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let text = if stdout.is_empty() { stderr } else { stdout };

            Ok(ProviderTurnOutput {
                output: if text.is_empty() {
                    "Claude completed without returning text output.".to_string()
                } else {
                    text
                },
                provider_state: session.provider_state.clone(),
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Err(if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("Claude CLI exited with status {}.", output.status)
            })
        }
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        Ok(format!(
            "Claude interrupt requested for session `{}`.",
            session.summary.title
        ))
    }
}

async fn probe_cli(
    binary: &str,
    kind: ProviderKind,
    version_args: &[&str],
    auth_args: &[&str],
) -> ProviderStatus {
    let label = kind.label();
    match Command::new(binary).args(version_args).output().await {
        Ok(version_output) => {
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
                                Some(format!("{label} CLI is installed but not authenticated."))
                            })
                    };

                    ProviderStatus {
                        kind,
                        label: label.to_string(),
                        installed: true,
                        authenticated,
                        version,
                        status: if authenticated {
                            ProviderStatusLevel::Ready
                        } else {
                            ProviderStatusLevel::Warning
                        },
                        message,
                    }
                }
                Err(error) => ProviderStatus {
                    kind,
                    label: label.to_string(),
                    installed: true,
                    authenticated: false,
                    version,
                    status: ProviderStatusLevel::Warning,
                    message: Some(format!(
                        "{label} CLI is installed, but auth probing failed: {error}"
                    )),
                },
            }
        }
        Err(error) => ProviderStatus {
            kind,
            label: label.to_string(),
            installed: false,
            authenticated: false,
            version: None,
            status: ProviderStatusLevel::Error,
            message: Some(format!("{label} CLI is unavailable: {error}")),
        },
    }
}

fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}
