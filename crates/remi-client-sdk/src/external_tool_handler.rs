use async_trait::async_trait;
use serde_json::Value as JsonValue;

use crate::chat_types::RichHandlerResult;

#[async_trait]
pub trait ExternalToolHandler: Send + Sync {
    async fn handle(&self, tool_call_id: &str, payload: &JsonValue) -> Result<JsonValue, String>;

    async fn handle_rich(
        &self,
        tool_call_id: &str,
        payload: &JsonValue,
    ) -> Result<RichHandlerResult, String> {
        self.handle(tool_call_id, payload)
            .await
            .map(RichHandlerResult::Json)
    }
}
