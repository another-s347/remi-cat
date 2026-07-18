#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;

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
    Acp,
}

impl ChannelKind {
    pub(crate) fn as_platform(self) -> &'static str {
        match self {
            Self::Feishu => "feishu",
            Self::Cli => "cli",
            Self::Tui => "tui",
            Self::Web => "web",
            Self::Acp => "acp",
        }
    }
}

pub(crate) trait Channel {
    fn kind(&self) -> ChannelKind;

    fn run<'a>(
        &'a self,
        runtime: std::rc::Rc<Runtime>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;
}
