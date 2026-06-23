use remi_agentloop::prelude::Message;

#[derive(Debug, Clone)]
struct PartialToolActivity {
    id: String,
    name: String,
    arguments_delta: String,
    arguments: Option<serde_json::Value>,
    result: Option<String>,
    success: Option<bool>,
    elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct PartialTurnRecorder {
    base_messages: Vec<Message>,
    assistant_text: String,
    reasoning_content: Option<String>,
    tools: Vec<PartialToolActivity>,
}

impl PartialTurnRecorder {
    pub(crate) fn new(base_messages: Vec<Message>) -> Self {
        Self {
            base_messages,
            assistant_text: String::new(),
            reasoning_content: None,
            tools: Vec::new(),
        }
    }

    pub(crate) fn on_text(&mut self, text: &str) {
        self.assistant_text.push_str(text);
    }

    pub(crate) fn on_thinking(&mut self, content: String) {
        self.reasoning_content = Some(content);
    }

    pub(crate) fn on_tool_start(&mut self, id: String, name: String) {
        if let Some(tool) = self.tools.iter_mut().find(|tool| tool.id == id) {
            tool.name = name;
            return;
        }
        self.tools.push(PartialToolActivity {
            id,
            name,
            arguments_delta: String::new(),
            arguments: None,
            result: None,
            success: None,
            elapsed_ms: None,
        });
    }

    pub(crate) fn on_tool_arguments_delta(&mut self, id: &str, delta: &str) {
        if let Some(tool) = self.tools.iter_mut().find(|tool| tool.id == id) {
            tool.arguments_delta.push_str(delta);
        }
    }

    pub(crate) fn on_tool_call(&mut self, id: String, name: String, args: serde_json::Value) {
        self.on_tool_start(id.clone(), name);
        if let Some(tool) = self.tools.iter_mut().find(|tool| tool.id == id) {
            tool.arguments = Some(args);
        }
    }

    pub(crate) fn on_tool_result(
        &mut self,
        id: String,
        name: String,
        args: serde_json::Value,
        result: String,
        success: bool,
        elapsed_ms: u64,
    ) {
        self.on_tool_call(id.clone(), name, args);
        if let Some(tool) = self.tools.iter_mut().find(|tool| tool.id == id) {
            tool.result = Some(result);
            tool.success = Some(success);
            tool.elapsed_ms = Some(elapsed_ms);
        }
    }

    pub(crate) fn synthesize_history(&self) -> Option<Vec<Message>> {
        if self.assistant_text.is_empty()
            && self.reasoning_content.is_none()
            && self.tools.is_empty()
        {
            return (!self.base_messages.is_empty()).then(|| self.base_messages.clone());
        }

        let mut messages = self.base_messages.clone();
        let mut content = self.assistant_text.clone();
        if !self.tools.is_empty() {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str("[Cancelled tool activity]");
            for tool in &self.tools {
                content.push_str("\n- ");
                content.push_str(&format!("{} ({})", tool.name, tool.id));
                let args = tool
                    .arguments
                    .as_ref()
                    .map(|value| value.to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| tool.arguments_delta.clone());
                if !args.trim().is_empty() {
                    content.push_str(&format!(" args: {}", args));
                }
                match (&tool.result, tool.success, tool.elapsed_ms) {
                    (Some(result), Some(success), Some(elapsed_ms)) => {
                        content.push_str(&format!(
                            " result: {} success: {} elapsed_ms: {}",
                            result, success, elapsed_ms
                        ));
                    }
                    _ => content.push_str(" status: cancelled before completion"),
                }
            }
        }
        if content.is_empty() {
            content = "[Cancelled before assistant text was produced]".to_string();
        }

        let mut message = Message::assistant(content);
        message.reasoning_content = self.reasoning_content.clone();
        messages.push(message);
        Some(messages)
    }
}
