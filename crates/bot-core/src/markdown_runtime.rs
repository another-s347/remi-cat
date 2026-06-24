//! Markdown-based agent runtime.
//!
//! This is the composition surface for the current Agent.md/Soul.md runtime:
//! markdown identity/context, skill injection, memory persistence, tool
//! execution, sandboxing, model providers, and supervisor workflows are wired
//! together by [`CatBot`].

pub use crate::runtime::{
    CatBot, CatBotBuilder, EffectiveModelProfile, EffectiveModelSource, RegisteredToolStatus,
    StreamOptions,
};
pub use crate::{AgentModelBindings, AgentProfile, AgentRegistry};
