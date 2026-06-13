use std::sync::Arc;

use async_trait::async_trait;
use tokio_stream::StreamExt;

use crate::chat_client::ChatClient;
use crate::chat_client::proto as chat_proto;
use crate::chat_types::{ChatRuntimeBackend, ChatRuntimeConfig};
use crate::local_wasm::{self, ChatEventStream};

#[async_trait]
pub(crate) trait ChatAgent: Send + Sync {
    async fn open_stream(
        &self,
        access_token: &str,
        config: &ChatRuntimeConfig,
        session_id: String,
        start_input: Option<chat_proto::ChatStartInput>,
        resume_input: Option<chat_proto::ChatResumeInput>,
    ) -> Result<ChatEventStream, String>;

    fn backend_name(&self) -> &'static str;
}

pub(crate) type SharedChatAgent = Arc<dyn ChatAgent>;

#[derive(Default)]
struct RemoteServerChatAgent;

struct LocalWasmChatAgent {
    agent: Arc<remi_agentloop_wasm::WasmAgentWithHttp>,
}

pub(crate) fn default_chat_agent() -> SharedChatAgent {
    Arc::new(RemoteServerChatAgent)
}

pub(crate) fn build_chat_agent(config: &ChatRuntimeConfig) -> Result<SharedChatAgent, String> {
    match &config.backend {
        ChatRuntimeBackend::RemoteServer => Ok(default_chat_agent()),
        ChatRuntimeBackend::LocalWasm(local) => Ok(Arc::new(LocalWasmChatAgent {
            agent: local_wasm::load_agent(&local.source)?,
        })),
    }
}

#[async_trait]
impl ChatAgent for RemoteServerChatAgent {
    async fn open_stream(
        &self,
        access_token: &str,
        _config: &ChatRuntimeConfig,
        session_id: String,
        start_input: Option<chat_proto::ChatStartInput>,
        resume_input: Option<chat_proto::ChatResumeInput>,
    ) -> Result<ChatEventStream, String> {
        let current_access_token = crate::auth::auth_get_access_token()
            .await
            .unwrap_or_else(|| access_token.to_string());

        tracing::info!(
            session_id = %session_id,
            "[ChatRuntime] opening remote server-hosted model stream"
        );
        tracing::info!(session_id = %session_id, "[ChatRuntime] creating ChatClient via shared transport");
        let mut client = ChatClient::new_with_shared_transport(current_access_token)
            .await
            .map_err(|e| {
                tracing::error!(session_id = %session_id, error = %e, "[ChatRuntime] failed to create ChatClient");
                format!("Failed to create chat client: {}", e)
            })?;
        tracing::info!(session_id = %session_id, "[ChatRuntime] ChatClient created, starting remote stream");

        let stream = client
            .chat_streaming_multimodal(session_id, start_input, resume_input)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "[ChatRuntime] failed to start remote stream");
                format!("Failed to start stream: {}", e)
            })?
            .map(|item| item.map_err(|error| error.to_string()));

        Ok(Box::pin(stream))
    }

    fn backend_name(&self) -> &'static str {
        "remote"
    }
}

#[async_trait]
impl ChatAgent for LocalWasmChatAgent {
    async fn open_stream(
        &self,
        _access_token: &str,
        config: &ChatRuntimeConfig,
        session_id: String,
        start_input: Option<chat_proto::ChatStartInput>,
        resume_input: Option<chat_proto::ChatResumeInput>,
    ) -> Result<ChatEventStream, String> {
        tracing::info!(session_id = %session_id, "[ChatRuntime] starting local WASM chat stream");
        local_wasm::start_stream(
            self.agent.clone(),
            config,
            &session_id,
            start_input,
            resume_input,
        )
        .await
    }

    fn backend_name(&self) -> &'static str {
        "local-wasm"
    }
}
