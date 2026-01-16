use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use futures::future::join_all;
use tokio::sync::broadcast;
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, ProviderAdapter, ProviderKind, RuntimeEvent,
    ServerMessage, SessionDetail, TurnStatus,
};

pub struct RuntimeCore {
    adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    orchestration: Arc<OrchestrationService>,
    persistence: Arc<PersistenceService>,
}

impl RuntimeCore {
    pub fn new(
        adapters: Vec<Arc<dyn ProviderAdapter>>,
        orchestration: Arc<OrchestrationService>,
        persistence: Arc<PersistenceService>,
    ) -> Self {
        let adapters = adapters
            .into_iter()
            .map(|adapter| (adapter.kind(), adapter))
            .collect::<HashMap<_, _>>();
        let (event_tx, _) = broadcast::channel(128);
        
        let registered: Vec<_> = adapters.keys().map(|k| k.label()).collect();
        tracing::info!(?registered, "Registered provider adapters");
        
        Self {
            adapters,
            event_tx,
            orchestration,
            persistence,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.event_tx.subscribe()
    }

    pub fn publish(&self, event: RuntimeEvent) {
        let _ = self.event_tx.send(event);
    }

    pub async fn snapshot(&self) -> AppSnapshot {
        AppSnapshot {
            generated_at: Utc::now().to_rfc3339(),
            sessions: self.persistence.list_sessions().await,
        }
    }

    pub async fn bootstrap(&self, ws_url: String) -> BootstrapPayload {
        let providers = join_all(self.adapters.values().map(|adapter| adapter.health())).await;

        BootstrapPayload {
            app_name: "zenui".to_string(),
            generated_at: Utc::now().to_rfc3339(),
            ws_url,
            providers,
            snapshot: self.snapshot().await,
        }
    }

    pub async fn handle_client_message(&self, message: ClientMessage) -> Option<ServerMessage> {
        tracing::debug!(?message, "Received client message");
        match message {
            ClientMessage::Ping => Some(ServerMessage::Pong),
            ClientMessage::LoadSnapshot => Some(ServerMessage::Snapshot {
                snapshot: self.snapshot().await,
            }),
            ClientMessage::StartSession { provider, title } => {
                tracing::info!(?provider, "Starting session");
                match self.start_session(provider, title).await {
                    Ok(session) => Some(ServerMessage::SessionCreated {
                        session: session.summary,
                    }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::SendTurn { session_id, input } => {
                match self.send_turn(session_id, input).await {
                    Ok(message) => Some(ServerMessage::Ack { message }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::InterruptTurn { session_id } => {
                match self.interrupt_turn(session_id).await {
                    Ok(message) => Some(ServerMessage::Ack { message }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
        }
    }

    async fn start_session(
        &self,
        provider: ProviderKind,
        title: Option<String>,
    ) -> Result<SessionDetail, String> {
        tracing::info!(?provider, "Looking up adapter for provider");
        let available: Vec<_> = self.adapters.keys().map(|k| k.label()).collect();
        tracing::debug!(?available, "Available adapters");
        
        let adapter = self
            .adapters
            .get(&provider)
            .ok_or_else(|| {
                tracing::error!(?provider, ?available, "Adapter not found for provider");
                format!("No adapter registered for {}.", provider.label())
            })?
            .clone();

        let mut session = self.orchestration.create_session(provider, title);
        session.provider_state = adapter.start_session(&session).await?;
        self.persistence.upsert_session(session.clone()).await;
        self.publish(RuntimeEvent::SessionStarted {
            session: session.summary.clone(),
        });
        Ok(session)
    }

    async fn send_turn(&self, session_id: String, input: String) -> Result<String, String> {
        let trimmed = input.trim().to_string();
        if trimmed.is_empty() {
            return Err("Turn input cannot be empty.".to_string());
        }

        let mut session = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;
        let adapter = self
            .adapters
            .get(&session.summary.provider)
            .ok_or_else(|| {
                format!(
                    "No adapter registered for {}.",
                    session.summary.provider.label()
                )
            })?
            .clone();

        let turn = self.orchestration.start_turn(&mut session, trimmed.clone());
        self.persistence.upsert_session(session.clone()).await;
        self.publish(RuntimeEvent::TurnStarted {
            session_id: session.summary.session_id.clone(),
            turn: turn.clone(),
        });

        match adapter.execute_turn(&session, &trimmed).await {
            Ok(output) => {
                if output.provider_state.is_some() {
                    session.provider_state = output.provider_state.clone();
                }
                let completed_turn = self
                    .orchestration
                    .finish_turn(
                        &mut session,
                        &turn.turn_id,
                        output.output.clone(),
                        TurnStatus::Completed,
                    )
                    .ok_or_else(|| format!("Unknown turn `{}`.", turn.turn_id))?;

                self.persistence.upsert_session(session.clone()).await;
                self.publish(RuntimeEvent::ContentDelta {
                    session_id: session.summary.session_id.clone(),
                    turn_id: completed_turn.turn_id.clone(),
                    delta: completed_turn.output.clone(),
                    accumulated_output: completed_turn.output.clone(),
                });
                self.publish(RuntimeEvent::TurnCompleted {
                    session_id: session.summary.session_id.clone(),
                    session: session.summary.clone(),
                    turn: completed_turn,
                });

                Ok("Turn completed.".to_string())
            }
            Err(error) => {
                let failed_turn = self
                    .orchestration
                    .finish_turn(
                        &mut session,
                        &turn.turn_id,
                        error.clone(),
                        TurnStatus::Failed,
                    )
                    .ok_or_else(|| format!("Unknown turn `{}`.", turn.turn_id))?;

                self.persistence.upsert_session(session.clone()).await;
                self.publish(RuntimeEvent::Error {
                    message: failed_turn.output.clone(),
                });
                self.publish(RuntimeEvent::TurnCompleted {
                    session_id: session.summary.session_id.clone(),
                    session: session.summary.clone(),
                    turn: failed_turn,
                });

                Err(error)
            }
        }
    }

    async fn interrupt_turn(&self, session_id: String) -> Result<String, String> {
        let mut session = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;
        let adapter = self
            .adapters
            .get(&session.summary.provider)
            .ok_or_else(|| {
                format!(
                    "No adapter registered for {}.",
                    session.summary.provider.label()
                )
            })?
            .clone();

        let message = adapter.interrupt_turn(&session).await?;
        self.orchestration.interrupt_session(&mut session, &message);
        self.persistence.upsert_session(session.clone()).await;
        self.publish(RuntimeEvent::SessionInterrupted {
            session: session.summary.clone(),
            message: message.clone(),
        });
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use zenui_orchestration::OrchestrationService;
    use zenui_persistence::PersistenceService;
    use zenui_provider_api::{
        ClientMessage, ProviderAdapter, ProviderKind, ProviderStatus, ProviderStatusLevel,
        ProviderTurnOutput, SessionDetail,
    };

    use super::RuntimeCore;

    struct FakeAdapter;

    #[async_trait]
    impl ProviderAdapter for FakeAdapter {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Codex
        }

        async fn health(&self) -> ProviderStatus {
            ProviderStatus {
                kind: ProviderKind::Codex,
                label: "Codex".to_string(),
                installed: true,
                authenticated: true,
                version: Some("test".to_string()),
                status: ProviderStatusLevel::Ready,
                message: None,
            }
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            input: &str,
        ) -> Result<ProviderTurnOutput, String> {
            Ok(ProviderTurnOutput {
                output: format!("fake response for {input}"),
                provider_state: None,
            })
        }
    }

    #[tokio::test]
    async fn creates_session_and_turn_snapshot() {
        let runtime = RuntimeCore::new(
            vec![Arc::new(FakeAdapter)],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
        );

        let response = runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
                title: Some("Test Session".to_string()),
            })
            .await;
        assert!(matches!(
            response,
            Some(zenui_provider_api::ServerMessage::SessionCreated { .. })
        ));

        let snapshot = runtime.snapshot().await;
        let session = snapshot.sessions.first().expect("session should exist");
        assert_eq!(session.summary.title, "Test Session");

        let response = runtime
            .handle_client_message(ClientMessage::SendTurn {
                session_id: session.summary.session_id.clone(),
                input: "hello".to_string(),
            })
            .await;
        assert!(matches!(
            response,
            Some(zenui_provider_api::ServerMessage::Ack { .. })
        ));

        let snapshot = runtime.snapshot().await;
        let session = snapshot.sessions.first().expect("session should exist");
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].output, "fake response for hello");
    }
}
