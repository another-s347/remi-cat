use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use remi_agentloop::prelude::{
    AgentError, ChatCtx as ToolContext, DefaultToolRegistry, ResumePayload, Tool, ToolDefinition,
    ToolDefinitionContext, ToolOutput, ToolResult,
};
use remi_agentloop::tool::FunctionDefinition;

type DynamicToolFuture =
    BoxFuture<'static, Result<ToolResult<BoxStream<'static, ToolOutput>>, AgentError>>;

type DynamicToolHandler = Arc<
    dyn Fn(serde_json::Value, Option<ResumePayload>, ToolContext) -> DynamicToolFuture
        + Send
        + Sync,
>;

pub struct DynamicTool {
    definition: ToolDefinition,
    handler: DynamicToolHandler,
}

impl DynamicTool {
    pub fn new<H, F, S>(definition: ToolDefinition, handler: H) -> Self
    where
        H: Fn(serde_json::Value, Option<ResumePayload>, ToolContext) -> F + Send + Sync + 'static,
        F: Future<Output = Result<ToolResult<S>, AgentError>> + Send + 'static,
        S: futures::Stream<Item = ToolOutput> + Send + 'static,
    {
        let handler = Arc::new(move |args, resume, ctx| {
            let fut = handler(args, resume, ctx);
            Box::pin(async move { Ok(fut.await?.map_stream(|stream| stream.boxed())) })
                as DynamicToolFuture
        });
        Self {
            definition,
            handler,
        }
    }

    pub fn from_parts<H, F, S>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        handler: H,
    ) -> Self
    where
        H: Fn(serde_json::Value, Option<ResumePayload>, ToolContext) -> F + Send + Sync + 'static,
        F: Future<Output = Result<ToolResult<S>, AgentError>> + Send + 'static,
        S: futures::Stream<Item = ToolOutput> + Send + 'static,
    {
        Self::new(
            ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDefinition {
                    name: name.into(),
                    description: description.into(),
                    parameters,
                    extra_prompt: None,
                },
            },
            handler,
        )
    }
}

impl Tool for DynamicTool {
    fn name(&self) -> &str {
        &self.definition.function.name
    }

    fn description(&self) -> &str {
        &self.definition.function.description
    }

    fn extra_prompt(&self, _ctx: &ToolDefinitionContext) -> Option<String> {
        self.definition.function.extra_prompt.clone()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.definition.function.parameters.clone()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        resume: Option<ResumePayload>,
        ctx: ToolContext,
    ) -> Result<ToolResult<impl futures::Stream<Item = ToolOutput> + 'static>, AgentError> {
        (self.handler)(arguments, resume, ctx).await
    }
}

pub fn register_dynamic_tool_definitions<H, F, S>(
    registry: &mut DefaultToolRegistry,
    definitions: impl IntoIterator<Item = ToolDefinition>,
    handler: H,
) where
    H: Fn(String, serde_json::Value, Option<ResumePayload>, ToolContext) -> F
        + Clone
        + Send
        + Sync
        + 'static,
    F: Future<Output = Result<ToolResult<S>, AgentError>> + Send + 'static,
    S: futures::Stream<Item = ToolOutput> + Send + 'static,
{
    for definition in definitions {
        let name = definition.function.name.clone();
        let handler = handler.clone();
        registry.register(DynamicTool::new(definition, move |args, resume, ctx| {
            handler(name.clone(), args, resume, ctx)
        }));
    }
}
