use super::{ChatGptAuthManager, API_BASE};
use anyhow::{bail, ensure, Context, Result};
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    Agent, AgentError, ChatCtx, ChatResponseChunk, Content, ContentPart, Message, ModelRequest,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    time::Duration,
};

const NAMESPACE: &str = "remi_cat";
const MAX_EVENT: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct ChatGptResponsesClient {
    auth: ChatGptAuthManager,
    client: reqwest::Client,
    configuration_error: Option<String>,
    #[cfg(test)]
    endpoint: Option<String>,
}
impl ChatGptResponsesClient {
    pub fn new(auth: ChatGptAuthManager) -> Self {
        Self {
            auth,
            configuration_error: None,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .build()
                .expect("TLS client"),
            #[cfg(test)]
            endpoint: None,
        }
    }
    pub(crate) fn with_profile(mut self, profile: &crate::ModelProfileConfig) -> Self {
        self.configuration_error = profile.validate().err().map(|e| e.to_string());
        self
    }
    /// Strict, one-turn probe used by CLI and the approval reviewer.
    pub async fn complete_text(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        extra_body: serde_json::Map<String, Value>,
    ) -> Result<String> {
        let req = ModelRequest {
            model: model.into(),
            messages: vec![Message::system(system), Message::user(prompt)],
            tools: None,
            temperature: None,
            max_tokens: None,
            stream: true,
            stream_options: None,
            metadata: None,
            rate_limit_retry: None,
            extra_body,
        };
        let ctx = ChatCtx::default();
        let mut stream = self
            .chat(ctx, req)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let mut text = String::new();
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                ChatResponseChunk::Delta { content, .. } => text.push_str(&content),
                ChatResponseChunk::Done => done = true,
                ChatResponseChunk::Error(e) => bail!("{e}"),
                ChatResponseChunk::ToolCallStart { .. } => {
                    bail!("unexpected tool call in text-only request")
                }
                _ => {}
            }
        }
        ensure!(
            done && !text.trim().is_empty(),
            "ChatGPT inference did not complete with visible text"
        );
        Ok(text)
    }
    fn endpoint(&self) -> String {
        #[cfg(test)]
        if let Some(endpoint) = &self.endpoint {
            return endpoint.clone();
        }
        format!("{API_BASE}/responses")
    }
}
impl Agent for ChatGptResponsesClient {
    type Request = ModelRequest;
    type Response = ChatResponseChunk;
    type Error = AgentError;
    #[allow(refining_impl_trait)]
    async fn chat(
        &self,
        ctx: ChatCtx,
        req: ModelRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = ChatResponseChunk> + '_>>, AgentError> {
        if let Some(error) = &self.configuration_error {
            return Err(AgentError::other(error.clone()));
        }
        let (body, names) = request_body(&req).map_err(agent_error)?;
        let model = req.model.clone();
        let cancellation = ctx.runtime().cancellation();
        let stream = async_stream::stream! {
            let mut response = None;
            let mut state_dir = None;
            for attempt in 0..3 {
                if ctx.is_cancelled() { yield ChatResponseChunk::Error(AgentError::other("ChatGPT request cancelled")); return; }
                let (token, root) = match self.auth.access_grant().await {
                    Ok(grant) => grant,
                    Err(e) => { yield ChatResponseChunk::Error(agent_error(e)); return; }
                };
                let dir = root.join(format!("{:x}", Sha256::digest(model.as_bytes())));
                let mut request_body = body.clone();
                // Bind opaque continuation state to the same locked authorization snapshot as the token.
                if let Err(e) = restore_reasoning(&mut request_body, &dir) { yield ChatResponseChunk::Error(agent_error(e)); return; }
                state_dir = Some(dir);
                let result = tokio::select! {
                    _ = cancellation.cancelled() => { yield ChatResponseChunk::Error(AgentError::other("ChatGPT request cancelled")); return; },
                    v = tokio::time::timeout(Duration::from_secs(60), self.client.post(self.endpoint()).bearer_auth(token).json(&request_body).send()) => v,
                };
                match result {
                    Ok(Ok(r)) if r.status().is_success() => { response = Some(r); break; }
                    Ok(Ok(r)) => {
                        let status = r.status();
                        let request_id = r.headers().get("x-request-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
                        let value = match tokio::time::timeout(Duration::from_secs(15), r.json::<Value>()).await { Ok(Ok(v)) => v, _ => Value::Null };
                        let error = api_error(status.as_u16(), &value, &request_id);
                        if attempt < 2 && matches!(status.as_u16(), 502 | 503 | 504) && !is_terminal_code(error_code(&value)) {
                            tokio::time::sleep(Duration::from_millis(250 << attempt)).await; continue;
                        }
                        yield ChatResponseChunk::Error(error); return;
                    }
                    _ if attempt < 2 => { tokio::time::sleep(Duration::from_millis(250 << attempt)).await; }
                    _ => { yield ChatResponseChunk::Error(AgentError::other("ChatGPT request transport failed before stream opened")); return; }
                }
            }
            let Some(response) = response else { return; };
            let Some(state_dir) = state_dir else { return; };
            let mut bytes = response.bytes_stream();
            let mut decoder = SseDecoder::default();
            let mut events = ResponseEvents::new(names);
            loop {
                let next = tokio::select! {
                    v = tokio::time::timeout(Duration::from_secs(90), bytes.next()) => Some(v),
                    _ = cancellation.cancelled() => None,
                };
                let chunk = match next {
                    None => { yield ChatResponseChunk::Error(AgentError::other("ChatGPT stream cancelled")); return; }
                    Some(Ok(Some(Ok(chunk)))) => chunk,
                    Some(Ok(None)) => { yield ChatResponseChunk::Error(AgentError::other("ChatGPT stream ended without response.completed")); return; }
                    _ => { yield ChatResponseChunk::Error(AgentError::other("ChatGPT stream interrupted or timed out")); return; }
                };
                let frames = match decoder.push(&chunk) { Ok(v) => v, Err(e) => { yield ChatResponseChunk::Error(agent_error(e)); return; } };
                for frame in frames {
                    let chunks = match events.accept(&frame) { Ok(v) => v, Err(e) => { yield ChatResponseChunk::Error(agent_error(e)); return; } };
                    for chunk in chunks {
                        if matches!(chunk, ChatResponseChunk::Done) {
                            if ctx.is_cancelled() { yield ChatResponseChunk::Error(AgentError::other("ChatGPT stream cancelled")); return; }
                            if let Err(e) = save_reasoning(&state_dir, &events.reasoning, &events.call_ids) { yield ChatResponseChunk::Error(agent_error(e)); return; }
                            yield chunk; return;
                        }
                        let failed = matches!(chunk, ChatResponseChunk::Error(_));
                        yield chunk;
                        if failed { return; }
                    }
                }
            }
        };
        Ok(Box::pin(stream))
    }
}
fn agent_error(e: anyhow::Error) -> AgentError {
    AgentError::other(e.to_string())
}

fn wire_name(name: &str) -> String {
    if !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        name.into()
    } else {
        format!(
            "tool_{:.58}",
            format!("{:x}", Sha256::digest(name.as_bytes()))
        )
    }
}
fn request_body(req: &ModelRequest) -> Result<(Value, HashMap<String, String>)> {
    let mut input = Vec::new();
    let mut names = HashMap::new();
    for message in &req.messages {
        let role = serde_json::to_value(&message.role)?;
        let role = role.as_str().context("invalid role")?;
        if role == "tool" {
            let id = message
                .tool_call_id
                .as_deref()
                .context("tool result has no call ID")?;
            ensure!(!id.is_empty(), "tool result has empty call ID");
            input.push(json!({"type":"function_call_output","call_id":id,"output":text_content(&message.content)?}));
            continue;
        }
        let role = if role == "system" { "developer" } else { role };
        ensure!(
            matches!(role, "developer" | "user" | "assistant"),
            "unsupported message role"
        );
        let content = response_content(&message.content, role == "assistant")?;
        if !content.is_empty() {
            input.push(json!({"role":role,"content":content}));
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                ensure!(!call.id.is_empty(), "tool call has empty call ID");
                let name = wire_name(&call.function.name);
                input.push(json!({"type":"function_call","call_id":call.id,"namespace":NAMESPACE,"name":name,"arguments":call.function.arguments}));
            }
        }
    }
    let mut body = json!({"model":req.model,"input":input,"store":false,"stream":true,"include":["reasoning.encrypted_content"]});
    if let Some(tools) = req.tools.as_ref().filter(|t| !t.is_empty()) {
        let mut functions = Vec::new();
        for tool in tools {
            ensure!(
                tool.tool_type == "function",
                "ChatGPT supports only local function tools"
            );
            let f = &tool.function;
            let name = wire_name(&f.name);
            if let Some(old) = names.insert(name.clone(), f.name.clone()) {
                ensure!(old == f.name, "tool wire-name collision");
            }
            let description = match &f.extra_prompt {
                Some(extra) => format!("{}\n{}", f.description, extra),
                None => f.description.clone(),
            };
            functions.push(json!({"type":"function","name":name,"description":description,"parameters":f.parameters,"strict":false}));
        }
        body["tools"] = json!([{"type":"namespace","name":NAMESPACE,"description":"Tools executed by remi-cat on the user's host, subject to local permissions.","tools":functions}]);
    }
    for (key, value) in &req.extra_body {
        match key.as_str() {
            "reasoning_effort" => {
                ensure!(value.is_string(), "reasoning_effort must be a string");
                body["reasoning"]["effort"] = value.clone();
            }
            "reasoning" => {
                let r = value.as_object().context("reasoning must be an object")?;
                ensure!(
                    r.keys().all(|k| matches!(k.as_str(), "effort" | "summary")),
                    "unsupported reasoning parameter"
                );
                body["reasoning"] = value.clone();
            }
            _ => bail!("unsupported ChatGPT request option: {key}"),
        }
    }
    Ok((body, names))
}
fn text_content(content: &Content) -> Result<String> {
    match content {
        Content::Text(t) => Ok(t.clone()),
        Content::Parts(parts) => parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => Ok(text.clone()),
                _ => bail!("non-text tool output is unsupported by ChatGPT adapter"),
            })
            .collect::<Result<Vec<_>>>()
            .map(|p| p.join("\n")),
    }
}
fn response_content(content: &Content, assistant: bool) -> Result<Vec<Value>> {
    let text_type = if assistant {
        "output_text"
    } else {
        "input_text"
    };
    let parts = match content {
        Content::Text(t) => {
            return Ok(if t.is_empty() {
                vec![]
            } else {
                vec![json!({"type":text_type,"text":t})]
            });
        }
        Content::Parts(parts) => parts,
    };
    parts.iter().map(|part| match part {
        ContentPart::Text { text } => Ok(json!({"type":text_type,"text":text})),
        ContentPart::ImageUrl { image_url } if !assistant => Ok(json!({"type":"input_image","image_url":image_url.url,"detail":"auto"})),
        ContentPart::ImageBase64 { media_type, data } if !assistant => Ok(json!({"type":"input_image","image_url":format!("data:{media_type};base64,{data}"),"detail":"auto"})),
        _ => bail!("unsupported ChatGPT input: only text and user images are supported"),
    }).collect()
}
#[derive(Default)]
struct SseDecoder {
    bytes: Vec<u8>,
    data: Vec<String>,
}
impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.bytes.extend_from_slice(bytes);
        ensure!(
            self.bytes.len() + self.data.iter().map(String::len).sum::<usize>() <= MAX_EVENT,
            "ChatGPT SSE event exceeds size limit"
        );
        let mut frames = Vec::new();
        let mut consumed = 0;
        while let Some(end) = self.bytes[consumed..].iter().position(|b| *b == b'\n') {
            let end = consumed + end;
            let line = std::str::from_utf8(&self.bytes[consumed..end])?.trim_end_matches('\r');
            if line.is_empty() {
                if !self.data.is_empty() {
                    frames.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
            }
            consumed = end + 1;
        }
        self.bytes.drain(..consumed);
        Ok(frames)
    }
}
struct Call {
    id: String,
    name: String,
    arguments: String,
}
struct ResponseEvents {
    names: HashMap<String, String>,
    calls: HashMap<usize, Call>,
    reasoning: Vec<Value>,
    call_ids: Vec<String>,
    finished_calls: HashSet<usize>,
}
impl ResponseEvents {
    fn new(names: HashMap<String, String>) -> Self {
        Self {
            names,
            calls: HashMap::new(),
            reasoning: vec![],
            call_ids: vec![],
            finished_calls: HashSet::new(),
        }
    }
    fn accept(&mut self, frame: &str) -> Result<Vec<ChatResponseChunk>> {
        ensure!(
            frame != "[DONE]",
            "ChatGPT stream has no response.completed"
        );
        let v: Value = serde_json::from_str(frame).context("invalid ChatGPT SSE JSON")?;
        let mut chunks = Vec::new();
        match v["type"].as_str().context("missing Responses event type")? {
            "response.output_text.delta" | "response.refusal.delta" => {
                chunks.push(ChatResponseChunk::Delta {
                    content: v["delta"].as_str().context("text delta missing")?.into(),
                    role: None,
                })
            }
            "response.reasoning_summary_text.delta" => {
                chunks.push(ChatResponseChunk::ReasoningDelta {
                    content: v["delta"].as_str().context("summary delta missing")?.into(),
                })
            }
            "response.output_item.added" if v["item"]["type"] == "function_call" => {
                let item = &v["item"];
                let index = index(&v)?;
                ensure!(
                    !self.calls.contains_key(&index),
                    "duplicate tool output index"
                );
                let id = item["call_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .context("tool call ID missing")?
                    .to_owned();
                ensure!(!self.call_ids.contains(&id), "duplicate tool call ID");
                let name = item["name"].as_str().context("tool name missing")?;
                ensure!(
                    item["namespace"].as_str().is_none_or(|n| n == NAMESPACE),
                    "unknown tool namespace"
                );
                let name = name.strip_prefix("remi_cat.").unwrap_or(name);
                let original = self
                    .names
                    .get(name)
                    .context("model called an unknown tool")?
                    .clone();
                chunks.push(ChatResponseChunk::ToolCallStart {
                    index,
                    id: id.clone(),
                    name: original,
                });
                let arguments = item["arguments"].as_str().unwrap_or("").to_owned();
                if !arguments.is_empty() {
                    chunks.push(ChatResponseChunk::ToolCallDelta {
                        index,
                        arguments_delta: arguments.clone(),
                    });
                }
                self.call_ids.push(id.clone());
                self.calls.insert(
                    index,
                    Call {
                        id,
                        name: name.into(),
                        arguments,
                    },
                );
            }
            "response.function_call_arguments.delta" => {
                let index = index(&v)?;
                let delta = v["delta"].as_str().context("tool argument delta missing")?;
                let call = self
                    .calls
                    .get_mut(&index)
                    .context("tool argument arrived before call")?;
                call.arguments.push_str(delta);
                chunks.push(ChatResponseChunk::ToolCallDelta {
                    index,
                    arguments_delta: delta.into(),
                });
            }
            "response.function_call_arguments.done" => {
                let index = index(&v)?;
                let call = self
                    .calls
                    .get_mut(&index)
                    .context("unknown tool arguments completion")?;
                let args = v["arguments"]
                    .as_str()
                    .or(v["item"]["arguments"].as_str())
                    .context("completed tool arguments missing")?;
                if call.arguments.is_empty() {
                    call.arguments = args.into();
                    chunks.push(ChatResponseChunk::ToolCallDelta {
                        index,
                        arguments_delta: args.into(),
                    });
                } else {
                    ensure!(call.arguments == args, "tool argument stream mismatch");
                }
            }
            "response.output_item.done" if v["item"]["type"] == "reasoning" => {
                self.reasoning.push(v["item"].clone())
            }
            "response.output_item.done" if v["item"]["type"] == "function_call" => {
                let index = index(&v)?;
                let call = self.calls.get(&index).context("unknown completed tool")?;
                let item = &v["item"];
                ensure!(
                    item["call_id"].as_str() == Some(&call.id)
                        && item["name"]
                            .as_str()
                            .map(|name| name.strip_prefix("remi_cat.").unwrap_or(name))
                            == Some(&call.name)
                        && item["namespace"]
                            .as_str()
                            .is_none_or(|name| name == NAMESPACE)
                        && item["arguments"].as_str() == Some(&call.arguments),
                    "completed tool differs from stream"
                );
                self.finished_calls.insert(index);
            }
            "response.completed" => {
                ensure!(
                    v["response"]["status"]
                        .as_str()
                        .is_none_or(|s| s == "completed"),
                    "response terminal status mismatch"
                );
                // Subscription streams can omit assembled output in the terminal event.
                // In that case, require individual tool completion events instead.
                let output = v["response"]["output"]
                    .as_array()
                    .filter(|items| !items.is_empty());
                if let Some(output) = output {
                    ensure!(
                        output
                            .iter()
                            .filter(|v| v["type"] == "function_call")
                            .count()
                            == self.calls.len(),
                        "completed response tool set differs from stream"
                    );
                    self.reasoning = output
                        .iter()
                        .filter(|v| v["type"] == "reasoning")
                        .cloned()
                        .collect();
                    for item in output.iter().filter(|v| v["type"] == "function_call") {
                        let id = item["call_id"]
                            .as_str()
                            .context("completed tool has no call ID")?;
                        let call = self
                            .calls
                            .values()
                            .find(|c| c.id == id)
                            .context("tool missing from streamed events")?;
                        ensure!(
                            item["name"]
                                .as_str()
                                .map(|name| name.strip_prefix("remi_cat.").unwrap_or(name))
                                == Some(&call.name)
                                && item["arguments"].as_str() == Some(&call.arguments),
                            "completed tool differs from stream"
                        );
                    }
                } else {
                    ensure!(
                        self.calls
                            .keys()
                            .all(|index| self.finished_calls.contains(index)),
                        "tool stream ended without item completion"
                    );
                }
                for call in self.calls.values() {
                    serde_json::from_str::<Value>(&call.arguments)
                        .context("invalid completed tool arguments")?;
                }
                if let Some(usage) = v["response"]["usage"].as_object() {
                    let token = |name: &str| {
                        usage
                            .get(name)
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                            .min(u32::MAX as u64) as u32
                    };
                    chunks.push(ChatResponseChunk::Usage {
                        prompt_tokens: token("input_tokens"),
                        completion_tokens: token("output_tokens"),
                        total_tokens: token("total_tokens"),
                    });
                }
                chunks.push(ChatResponseChunk::Done);
            }
            "response.failed" | "response.incomplete" | "error" => {
                let detail = if v["type"] == "response.incomplete" {
                    "ChatGPT response incomplete".into()
                } else {
                    api_error(
                        0,
                        if v["response"].is_object() {
                            &v["response"]
                        } else {
                            &v
                        },
                        "",
                    )
                    .to_string()
                };
                chunks.push(ChatResponseChunk::Error(AgentError::other(detail)));
            }
            _ => {}
        }
        Ok(chunks)
    }
}
fn index(v: &Value) -> Result<usize> {
    Ok(v["output_index"]
        .as_u64()
        .context("tool output index missing")? as usize)
}
fn error_code(v: &Value) -> &str {
    v["error"]["code"]
        .as_str()
        .or(v["code"].as_str())
        .unwrap_or("unknown")
}
fn is_terminal_code(code: &str) -> bool {
    matches!(
        code,
        "subscription_sharing_user_not_eligible"
            | "subscription_sharing_usage_limit_exceeded"
            | "subscription_sharing_unsupported_capability"
            | "subscription_sharing_route_not_supported"
            | "subscription_sharing_invalid_user"
    )
}
fn api_error(status: u16, v: &Value, request_id: &str) -> AgentError {
    let code = error_code(v);
    let advice = match code {
        "subscription_sharing_usage_limit_exceeded" => {
            "ChatGPT usage limit reached; review https://chatgpt.com/settings/usage"
        }
        "subscription_sharing_user_not_eligible" => {
            "ChatGPT plan is unavailable for this account/workspace"
        }
        "subscription_sharing_unsupported_capability" => {
            "unsupported ChatGPT capability; adjust model options"
        }
        "subscription_sharing_route_not_supported" => "ChatGPT route not supported",
        "subscription_sharing_invalid_user" => "ChatGPT authorization invalid; log in again",
        _ => "ChatGPT request failed; no alternate billing path was used",
    };
    // Keep machine code/request ID, but do not log arbitrary server bodies.
    AgentError::other(format!(
        "{advice} (HTTP {status}; code={}; request_id={})",
        safe_label(code),
        safe_label(request_id)
    ))
}
fn safe_label(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(160)
        .collect()
}
fn state_file(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    dir.join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
}
fn restore_reasoning(body: &mut Value, dir: &std::path::Path) -> Result<()> {
    let original = body["input"].as_array().context("input array missing")?;
    let mut input = Vec::new();
    let mut seen = HashSet::new();
    for item in original {
        if item["type"] == "function_call" {
            let path = state_file(dir, item["call_id"].as_str().context("call ID missing")?);
            match std::fs::read(path) {
                Ok(bytes) => {
                    let reasoning: Vec<Value> = serde_json::from_slice(&bytes)
                        .context("invalid saved Responses reasoning")?;
                    for r in reasoning {
                        if let Some(id) = r["id"].as_str() {
                            if seen.insert(id.to_owned()) {
                                input.push(r);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        input.push(item.clone());
    }
    body["input"] = Value::Array(input);
    Ok(())
}
fn save_reasoning(dir: &std::path::Path, reasoning: &[Value], calls: &[String]) -> Result<()> {
    if reasoning.is_empty() || calls.is_empty() {
        return Ok(());
    }
    super::auth::secure_dir(dir)?;
    for id in calls {
        super::auth::write_private_json(&state_file(dir, id), &serde_json::to_vec(reasoning)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::HeaderMap,
        response::{
            sse::{Event, Sse},
            IntoResponse,
        },
        routing::post,
        Json, Router,
    };
    use remi_agentloop::tool::{FunctionDefinition, ToolDefinition};
    use remi_agentloop::types::{FunctionCall, ToolCallMessage};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::Mutex;
    fn request() -> ModelRequest {
        ModelRequest {
            model: "test-model".into(),
            messages: vec![Message::system("system"), Message::user("hello")],
            tools: None,
            temperature: None,
            max_tokens: Some(100),
            stream: true,
            stream_options: None,
            metadata: None,
            rate_limit_retry: None,
            extra_body: Default::default(),
        }
    }
    fn call_message() -> Message {
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCallMessage {
                id: "call-1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: "{\"path\":\"test.txt\"}".into(),
                },
            }],
            None,
        )
    }
    fn tool() -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "read_file".into(),
                description: "Read local file".into(),
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                extra_prompt: None,
            },
        }
    }
    fn terminal() -> Value {
        json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13},"output":[]}})
    }
    #[test]
    fn request_contract_preserves_roles_calls_images_and_rejects_options() {
        let mut req = request();
        req.tools = Some(vec![tool()]);
        req.messages.push(call_message());
        req.messages
            .push(Message::tool_result("call-1", "file contents"));
        req.extra_body
            .insert("reasoning".into(), json!({"effort":"high"}));
        let (body, names) = request_body(&req).unwrap();
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][2]["namespace"], NAMESPACE);
        assert_eq!(body["input"][3]["call_id"], "call-1");
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(names["read_file"], "read_file");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        for key in [
            "max_output_tokens",
            "max_tokens",
            "temperature",
            "previous_response_id",
            "conversation",
            "metadata",
        ] {
            assert!(body.get(key).is_none());
        }
        let mut image = Message::user("");
        image.content = Content::Parts(vec![ContentPart::ImageBase64 {
            media_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        }]);
        req.messages.push(image);
        let (body, _) = request_body(&req).unwrap();
        assert_eq!(body["input"][4]["content"][0]["type"], "input_image");
        for key in [
            "temperature",
            "max_output_tokens",
            "tool_search",
            "store",
            "stream",
            "previous_response_id",
        ] {
            req.extra_body.insert(key.into(), json!(true));
            assert!(request_body(&req).is_err());
            req.extra_body.remove(key);
        }
        let alias = wire_name("invalid/name.with.dots");
        assert!(alias.len() <= 64);
    }
    #[test]
    fn sse_handles_split_utf8_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        let data="event: response\r\ndata: {\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"你好\"}\r\n\r\n";
        let mut frames = vec![];
        for byte in data.as_bytes() {
            frames.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(frames.len(), 1);
        let mut events = ResponseEvents::new(Default::default());
        assert!(
            matches!(&events.accept(&frames[0]).unwrap()[0],ChatResponseChunk::Delta {content,..} if content=="你好")
        );
    }
    #[test]
    fn terminal_without_assembled_output_requires_completed_tool_items() {
        let mut events =
            ResponseEvents::new(HashMap::from([("read_file".into(), "read_file".into())]));
        let call = json!({"type":"function_call","call_id":"call-1","name":"read_file","namespace":NAMESPACE,"arguments":"{}"});
        events
            .accept(
                &json!({"type":"response.output_item.added","output_index":0,"item":call})
                    .to_string(),
            )
            .unwrap();
        assert!(events.accept(&terminal().to_string()).is_err());
        let invalid = json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"wrong","name":"read_file","arguments":"{}"}});
        assert!(events.accept(&invalid.to_string()).is_err());
        events
            .accept(
                &json!({"type":"response.output_item.done","output_index":0,"item":call})
                    .to_string(),
            )
            .unwrap();
        let reasoning = json!({"type":"reasoning","encrypted_content":"opaque-test"});
        events
            .accept(
                &json!({"type":"response.output_item.done","output_index":1,"item":reasoning})
                    .to_string(),
            )
            .unwrap();
        assert!(events
            .accept(&terminal().to_string())
            .unwrap()
            .iter()
            .any(|chunk| matches!(chunk, ChatResponseChunk::Done)));
        assert_eq!(events.reasoning, vec![reasoning]);
    }
    #[test]
    fn parser_requires_terminal_success_and_validates_tool_stream() {
        let mut e = ResponseEvents::new(HashMap::from([("read_file".into(), "read_file".into())]));
        let added = json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call-1","name":"read_file","namespace":NAMESPACE,"arguments":""}});
        assert!(matches!(
            e.accept(&added.to_string()).unwrap()[0],
            ChatResponseChunk::ToolCallStart { index: 2, .. }
        ));
        e.accept(
            &json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{}"})
                .to_string(),
        )
        .unwrap();
        assert!(e.accept(&json!({"type":"response.function_call_arguments.done","output_index":2,"arguments":"[]"}).to_string()).is_err());
        for event in [
            json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}),
            json!({"type":"response.incomplete"}),
            json!({"type":"error","code":"subscription_sharing_user_not_eligible"}),
        ] {
            assert!(matches!(
                e.accept(&event.to_string()).unwrap()[0],
                ChatResponseChunk::Error(_)
            ));
        }
        assert!(e.accept("[DONE]").is_err());
        assert!(matches!(
            ResponseEvents::new(Default::default())
                .accept(&terminal().to_string())
                .unwrap()
                .last(),
            Some(ChatResponseChunk::Done)
        ));
    }
    #[derive(Clone, Default)]
    struct Mock {
        requests: Arc<Mutex<Vec<Value>>>,
        mode: Arc<AtomicUsize>,
    }
    async fn endpoint(
        State(state): State<Mock>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        assert_eq!(headers["authorization"], "Bearer test-access");
        let mut requests = state.requests.lock().await;
        requests.push(body);
        let request_number = requests.len();
        drop(requests);
        if state.mode.load(Ordering::SeqCst) == 2 {
            return (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error":{"code":"subscription_sharing_usage_limit_exceeded"}})),
            )
                .into_response();
        }
        let events = if state.mode.load(Ordering::SeqCst) == 3 && request_number == 1 {
            let call = json!({"type":"function_call","call_id":"call-1","name":"read_file","namespace":NAMESPACE,"arguments":"{\"path\":\"test.txt\"}"});
            let reasoning = json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque-test-state"});
            vec![
                json!({"type":"response.output_item.added","output_index":1,"item":call}),
                json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
                json!({"type":"response.completed","response":{"status":"completed","output":[reasoning,call]}}),
            ]
        } else if state.mode.load(Ordering::SeqCst) == 1 {
            vec![json!({"type":"response.output_text.delta","delta":"partial"})]
        } else {
            vec![
                json!({"type":"response.output_text.delta","delta":"hello"}),
                terminal(),
            ]
        };
        Sse::new(futures::stream::iter(events.into_iter().map(|v| {
            Ok::<_, std::convert::Infallible>(Event::default().data(v.to_string()))
        })))
        .into_response()
    }
    async fn server() -> (
        tempfile::TempDir,
        ChatGptResponsesClient,
        Mock,
        tokio::task::JoinHandle<()>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let auth = super::super::auth::test_auth(dir.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint_url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let state = Mock::default();
        let app = Router::new()
            .route("/v1/responses", post(endpoint))
            .with_state(state.clone());
        let h = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut client = ChatGptResponsesClient::new(auth);
        client.endpoint = Some(endpoint_url);
        (dir, client, state, h)
    }
    #[tokio::test]
    async fn http_success_disconnect_quota_cancel_and_logout() {
        let (_dir, client, state, h) = server().await;
        assert_eq!(
            client
                .complete_text("test", "system", "hello", Default::default())
                .await
                .unwrap(),
            "hello"
        );
        state.mode.store(1, Ordering::SeqCst);
        assert!(client
            .complete_text("test", "system", "hello", Default::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("without response.completed"));
        state.mode.store(2, Ordering::SeqCst);
        assert!(client
            .complete_text("test", "system", "hello", Default::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("usage limit"));
        assert_eq!(state.requests.lock().await.len(), 3); // no retry on a usage-limit error
        let ctx = ChatCtx::default();
        ctx.cancel();
        let mut stream = client.chat(ctx, request()).await.unwrap();
        assert!(matches!(
            stream.next().await,
            Some(ChatResponseChunk::Error(_))
        ));
        drop(stream);
        assert_eq!(state.requests.lock().await.len(), 3);
        client.auth.logout().await.unwrap();
        assert!(client
            .complete_text("test", "system", "hello", Default::default())
            .await
            .is_err());
        assert_eq!(state.requests.lock().await.len(), 3);
        h.abort();
    }
    #[test]
    fn encrypted_reasoning_replays_once_before_parallel_tool_calls_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let reasoning =
            vec![json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque"})];
        save_reasoning(dir.path(), &reasoning, &["call-1".into(), "call-2".into()]).unwrap();
        let mut body = json!({"input":[{"type":"function_call","call_id":"call-1"},{"type":"function_call","call_id":"call-2"}]});
        restore_reasoning(&mut body, dir.path()).unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(body["input"][0]["encrypted_content"], "opaque");
    }
    #[tokio::test]
    async fn provider_rejects_custom_endpoint_before_network() {
        let (_dir, client, state, h) = server().await;
        let mut profile = crate::ModelProfileConfig {
            id: "test".into(),
            name: "test".into(),
            model: "test".into(),
            base_url: Some("https://attacker.invalid/v1".into()),
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 100,
            context_tokens: 1000,
            supports_images: false,
            legacy_short_term_tokens: None,
            overflow_bytes: 1000,
            context_compaction: crate::ContextCompactionMode::Off,
            description: None,
            provider: Some("chatgpt".into()),
            extra_options: Default::default(),
        };
        assert!(client
            .clone()
            .with_profile(&profile)
            .complete_text("test", "system", "hello", Default::default())
            .await
            .is_err());
        profile.base_url = None;
        profile.extra_options.insert("temperature".into(), json!(1));
        assert!(client
            .with_profile(&profile)
            .complete_text("test", "system", "hello", Default::default())
            .await
            .is_err());
        assert!(state.requests.lock().await.is_empty());
        h.abort();
    }
    struct ReadFixture(std::path::PathBuf);
    impl remi_agentloop::tool::Tool for ReadFixture {
        fn name(&self) -> &str {
            "read_file"
        }
        fn description(&self) -> &str {
            "Read the test fixture"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
        }
        async fn execute(
            &self,
            args: Value,
            _resume: Option<remi_agentloop::types::ResumePayload>,
            _ctx: ChatCtx,
        ) -> Result<
            remi_agentloop::tool::ToolResult<
                impl Stream<Item = remi_agentloop::tool::ToolOutput> + 'static,
            >,
            AgentError,
        > {
            if args["path"] != "test.txt" {
                return Err(AgentError::other("unexpected fixture path"));
            }
            let text = std::fs::read_to_string(self.0.join("test.txt"))
                .map_err(|_| AgentError::other("fixture unavailable"))?;
            Ok(remi_agentloop::tool::ToolResult::Output(
                futures::stream::once(async move { remi_agentloop::tool::ToolOutput::text(text) }),
            ))
        }
    }
    #[tokio::test]
    #[ignore = "Consumes subscription usage; set REMI_CHATGPT_LIVE_DATA_DIR explicitly"]
    async fn live_subscription_agent_tool_and_compression() {
        use remi_agentloop::prelude::{AgentBuilder, AgentConfig, LoopInput};
        use remi_agentloop::types::AgentEvent;
        let root = std::path::PathBuf::from(
            std::env::var("REMI_CHATGPT_LIVE_DATA_DIR")
                .expect("explicit live auth directory required"),
        );
        let profile: crate::ModelProfileConfig = serde_json::from_value(json!({
            "id":"live-subscription", "name":"Live subscription acceptance", "model":"gpt-6-luna",
            "provider":"chatgpt", "context_tokens":32768, "max_output_tokens":4096,
            "supports_images":false, "overflow_bytes":32768, "context_compaction":"hard"
        }))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let nonce = format!(
            "remi-live-{:x}",
            Sha256::digest(dir.path().as_os_str().as_encoded_bytes())
        );
        std::fs::write(dir.path().join("test.txt"), &nonce).unwrap();
        let agent = AgentBuilder::new()
            .model(crate::runtime::model_provider::build_provider_client_with_auth("", &profile, None, &root))
            .config(AgentConfig::default().with_model(&profile.model))
            .system("Use read_file to read test.txt. After the tool returns, reply with its exact contents. Never invent file contents.")
            .tool(ReadFixture(dir.path().into()))
            .max_turns(4).build_loop();
        let mut stream = agent
            .chat(
                ChatCtx::default(),
                LoopInput::start("Read test.txt and return its full contents."),
            )
            .await
            .unwrap();
        let mut done = false;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::TextDelta(delta) => text.push_str(&delta),
                AgentEvent::Done => done = true,
                AgentEvent::Error(error) => panic!("live agent failed: {error}"),
                _ => {}
            }
        }
        assert!(
            done && text.contains(&nonce),
            "live tool result was not read back"
        );
        println!("Live subscription AgentLoop: completed with real file tool readback.");
        let compressor = crate::memory::LlmCompressor::new(
            "".into(),
            None,
            profile.model.clone(),
            32768,
            4096,
            Default::default(),
        )
        .with_provider(profile, root);
        let summary = compressor
            .compress(&[
                Message::user(format!(
                    "Remember the exact verification identifier: {nonce}"
                )),
                Message::assistant("I will preserve that exact identifier."),
            ])
            .await
            .unwrap();
        assert!(!summary.trim().is_empty() && summary.contains(&nonce));
        println!(
            "Live subscription compressor: nonempty summary preserved verification identifier."
        );
    }
    #[tokio::test]
    async fn agent_loop_executes_tool_and_returns_result_with_reasoning_then_resumes() {
        use remi_agentloop::prelude::{AgentBuilder, AgentConfig, LoopInput};
        use remi_agentloop::types::AgentEvent;
        let (dir, client, state, h) = server().await;
        state.mode.store(3, Ordering::SeqCst);
        std::fs::write(dir.path().join("test.txt"), "real fixture contents").unwrap();
        let model = crate::runtime::model_provider::ProviderClient::ChatGpt {
            client: client.clone(),
            supports_multimodal: true,
        };
        let agent = AgentBuilder::new()
            .model(model)
            .config(AgentConfig::default().with_model("test-model"))
            .system("Read the requested file.")
            .tool(ReadFixture(dir.path().into()))
            .max_turns(3)
            .build_loop();
        let mut stream = agent
            .chat(ChatCtx::default(), LoopInput::start("read test.txt"))
            .await
            .unwrap();
        let mut done = false;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::TextDelta(t) => text.push_str(&t),
                AgentEvent::Done => done = true,
                AgentEvent::Error(e) => panic!("agent error: {e}"),
                _ => {}
            }
        }
        assert!(done);
        assert_eq!(text, "hello");
        let requests = state.requests.lock().await;
        assert_eq!(requests.len(), 2);
        let input = requests[1]["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["type"] == "function_call_output"
            && i["call_id"] == "call-1"
            && i["output"] == "real fixture contents"));
        assert!(input
            .iter()
            .any(|i| i["encrypted_content"] == "opaque-test-state"));
        drop(requests);
        // A new client reconstructs protocol state from disk and replays the tool pair.
        let mut restarted = ChatGptResponsesClient::new(ChatGptAuthManager::new(dir.path()));
        restarted.endpoint = client.endpoint.clone();
        let mut req = request();
        req.tools = Some(vec![tool()]);
        req.messages.extend([
            call_message(),
            Message::tool_result("call-1", "real fixture contents"),
            Message::user("what did you read?"),
        ]);
        let mut stream = restarted.chat(ChatCtx::default(), req).await.unwrap();
        while let Some(chunk) = stream.next().await {
            if let ChatResponseChunk::Error(e) = chunk {
                panic!("restart: {e}");
            }
        }
        let requests = state.requests.lock().await;
        assert!(requests[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["encrypted_content"] == "opaque-test-state"));
        h.abort();
    }
}
