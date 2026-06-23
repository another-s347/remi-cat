#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;

use bot_core::{ImAttachment, ImDocument, ToolApprovalDecision, ToolApprovalRequest};

use crate::core::Runtime;

pub(crate) mod cli;
pub(crate) mod feishu;
pub(crate) mod tui;
pub(crate) mod web;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChannelKind {
    Feishu,
    Cli,
    Tui,
    Web,
}

impl ChannelKind {
    pub(crate) fn as_platform(self) -> &'static str {
        match self {
            Self::Feishu => "feishu",
            Self::Cli => "cli",
            Self::Tui => "tui",
            Self::Web => "web",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChannelMessage {
    pub(crate) kind: ChannelKind,
    pub(crate) message_id: String,
    pub(crate) channel_id: String,
    pub(crate) user_id: String,
    pub(crate) username: Option<String>,
    pub(crate) text: String,
    pub(crate) chat_type: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) attachments: Vec<ImAttachment>,
    pub(crate) documents: Vec<ImDocument>,
}

impl ChannelMessage {
    pub(crate) fn platform(&self) -> &'static str {
        self.kind.as_platform()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelReplyKind {
    Text,
    Thinking,
    Tool,
    Supervisor,
    Stats,
    Error,
}

pub(crate) trait ChannelSink {
    fn send<'a>(
        &'a mut self,
        kind: ChannelReplyKind,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;

    fn tool_approval_requested<'a>(
        &'a mut self,
        _request: &'a ToolApprovalRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn tool_approval_resolved<'a>(
        &'a mut self,
        _request: &'a ToolApprovalRequest,
        _decision: ToolApprovalDecision,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

pub(crate) trait Channel {
    fn kind(&self) -> ChannelKind;

    fn run<'a>(
        &'a self,
        runtime: std::rc::Rc<Runtime>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;
}
