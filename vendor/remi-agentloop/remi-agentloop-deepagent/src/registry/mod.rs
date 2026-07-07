//! `FileBackedRegistry` — a tool registry wrapper that auto-spills long tool
//! outputs to files instead of stranding them in the LLM context window.
//!
//! When a tool's `ToolOutput::Result` exceeds `threshold_bytes`, the content is
//! written to `output_dir/<tool_call_id>.txt` and a pointer message is returned.
//! The path in the message is relative to `workspace_root` so `fs_read` can
//! resolve it directly:
//!
//! ```text
//! [Output too long (12345 bytes). Full content saved to .tool-results/abc.txt.
//!  Use fs_read with path=".tool-results/abc.txt", offset=0, length=4096 to read
//!  the first chunk, then increment offset by 4096 until remaining=0.]
//! ```

use futures::StreamExt;
use remi_core::error::AgentError;
use remi_core::tool::registry::ToolRegistry;
use remi_core::tool::{
    BoxedToolResult, ToolContext, ToolDefinition, ToolDefinitionContext, ToolOutput,
};
use remi_core::types::{ParsedToolCall, ResumePayload};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

// ── FileBackedRegistry ────────────────────────────────────────────────────────

/// Wraps any [`ToolRegistry`] and captures oversized outputs to disk.
pub struct FileBackedRegistry<R> {
    inner: R,
    /// Output file length threshold in bytes (default 4 KiB).
    pub threshold_bytes: usize,
    /// Directory where oversized outputs are written (default `.deepagent/tool-results`).
    pub output_dir: PathBuf,
    /// Workspace root — used to make paths in spill messages relative so `fs_read` can resolve them.
    pub workspace_root: Option<PathBuf>,
}

impl<R: ToolRegistry> FileBackedRegistry<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            threshold_bytes: 4096,
            output_dir: PathBuf::from(".deepagent/tool-results"),
            workspace_root: None,
        }
    }

    pub fn threshold(mut self, bytes: usize) -> Self {
        self.threshold_bytes = bytes;
        self
    }

    pub fn output_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.output_dir = dir.into();
        self
    }

    pub fn workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
        self
    }
}

impl<R: ToolRegistry> ToolRegistry for FileBackedRegistry<R> {
    fn definitions(&self, user_state: &serde_json::Value) -> Vec<ToolDefinition> {
        self.inner.definitions(user_state)
    }

    fn definitions_with_context(&self, ctx: &ToolDefinitionContext) -> Vec<ToolDefinition> {
        self.inner.definitions_with_context(ctx)
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn contains(&self, name: &str) -> bool {
        self.inner.contains(name)
    }

    fn execute_parallel<'a>(
        &'a self,
        calls: &'a [ParsedToolCall],
        resume_map: &'a HashMap<String, ResumePayload>,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Vec<(String, Result<BoxedToolResult<'a>, AgentError>)>> + 'a>>
    {
        Box::pin(async move {
            let inner_results = self.inner.execute_parallel(calls, resume_map, ctx).await;

            let mut out = Vec::with_capacity(inner_results.len());

            for (call_id, result) in inner_results {
                let new_result: Result<BoxedToolResult<'a>, AgentError> = match result {
                    Err(e) => Err(e),
                    Ok(remi_core::tool::ToolResult::Interrupt(req)) => {
                        Ok(remi_core::tool::ToolResult::Interrupt(req))
                    }
                    Ok(remi_core::tool::ToolResult::Output(mut stream)) => {
                        // Collect the full stream
                        let mut deltas: Vec<String> = Vec::new();
                        let mut final_result: Option<String> = None;
                        while let Some(item) = stream.next().await {
                            match item {
                                ToolOutput::Delta(d) => deltas.push(d),
                                ToolOutput::SubSession(_) => {}
                                ToolOutput::Result(c) => final_result = Some(c.text_content()),
                            }
                        }

                        let new_result_str = if let Some(result_str) = final_result {
                            if result_str.len() > self.threshold_bytes {
                                // Write to file
                                let short_id = &call_id[..call_id.len().min(8)];
                                let fname = format!("{short_id}.txt");
                                let fpath = self.output_dir.join(&fname);
                                match spill_to_file(&fpath, &result_str).await {
                                    Ok(()) => {
                                        let total = result_str.len();
                                        let chunk = self.threshold_bytes;
                                        // Show a workspace-relative path so fs_read can resolve it
                                        let rel_path = self
                                            .workspace_root
                                            .as_deref()
                                            .and_then(|root| fpath.strip_prefix(root).ok())
                                            .unwrap_or(&fpath);
                                        format!(
                                            "[Output too long ({total} bytes). \
                                             Full content saved to {rel}. \
                                             Use fs_read with path=\"{rel}\", offset=0, length={chunk} \
                                             to read the first chunk, then increment offset by {chunk} \
                                             until remaining=0.]",
                                            rel = rel_path.display(),
                                        )
                                    }
                                    Err(_) => result_str, // fallback: return as-is
                                }
                            } else {
                                result_str
                            }
                        } else {
                            String::new()
                        };

                        // Reconstruct a static stream
                        let new_result_str: String = new_result_str;
                        let deltas_owned: Vec<String> = deltas;
                        let s = async_stream::stream! {
                            for d in deltas_owned {
                                yield ToolOutput::Delta(d);
                            }
                            yield ToolOutput::text(new_result_str);
                        };
                        let boxed: remi_core::tool::BoxedToolStream<'a> = Box::pin(s);
                        Ok(remi_core::tool::ToolResult::Output(boxed))
                    }
                };
                out.push((call_id, new_result));
            }

            out
        })
    }
}

async fn spill_to_file(path: &Path, content: &str) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, content).await
}
