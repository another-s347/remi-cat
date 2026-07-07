//! # remi-agentloop
//!
//! Facade crate — re-exports everything from the `remi-agentloop-*` sub-crates
//! so users can add a single dependency instead of listing each sub-crate.
//!
//! ## Usage
//!
//! ```toml
//! # Cargo.toml
//! [dependencies]
//! remi-agentloop = { version = "*", features = ["http-client"] }
//! ```
//!
//! ```ignore
//! use remi_agentloop::prelude::*;
//!
//! let agent = AgentBuilder::new()
//!     .model(OpenAIClient::from_env())
//!     .system("You are a helpful assistant.")
//!     .max_turns(10)
//!     .build();
//!
//! let mut stream = agent.chat("Hello!".into()).await?;
//! // …
//! ```
//!
//! ## Feature flags
//!
//! | Feature | Description |
//! |---------|-------------|
//! | `http-client` | Enable [`ReqwestTransport`] and `OpenAIClient::from_env()` |
//! | `tool-bash` | Enable [`BashTool`] |
//! | `tool-fs` | Enable [`FsTool`] |
//! | `tool-bash-virtual` | In-process sandboxed bash |
//! | `tool-fs-virtual` | In-process sandboxed filesystem |
//! | `tracing-langsmith` | Enable [`LangSmithTracer`] |
//!
//! For fine-grained control, depend on the individual sub-crates directly
//! (`remi-agentloop-core`, `remi-agentloop-model`, etc.).

// ── Re-exports from remi-core ─────────────────────────────────────────────────

pub use remi_agentloop_macros::tool as tool_macro;

pub use remi_core::{
    adapters, agent, agent_loop, builder, checkpoint, config, context, error, interrupt, model,
    protocol, state, tool, tracing, types, union,
};

// ── Re-exports from remi-transport ────────────────────────────────────────────

/// HTTP transport abstraction (HttpTransport trait, ReqwestTransport, SSE)
pub mod transport {
    pub use remi_transport::*;
}

/// HTTP transport abstraction — re-exported module
pub mod http {
    pub use remi_transport::http::*;
}

// ── Re-exports from remi-model ────────────────────────────────────────────────

/// OpenAI-compatible model implementations
pub mod openai {
    pub use remi_model::openai::*;
}

// ── Prelude ───────────────────────────────────────────────────────────────────

pub mod prelude {
    // Core
    pub use remi_core::prelude::*;

    // Transport
    pub use remi_transport::HttpTransport;
    #[cfg(feature = "http-client")]
    pub use remi_transport::ReqwestTransport;

    // Model
    pub use remi_model::OpenAIClient;

    // Tracing
    #[cfg(feature = "tracing-langsmith")]
    pub use remi_core::tracing::LangSmithTracer;

    // Tool implementations
    #[cfg(feature = "tool-bash")]
    pub use remi_tool::BashTool;
    #[cfg(feature = "tool-fs")]
    pub use remi_tool::FsTool;
    #[cfg(feature = "tool-bash-virtual")]
    pub use remi_tool::VirtualBashTool;
    #[cfg(feature = "tool-fs-virtual")]
    pub use remi_tool::VirtualFsTool;
}
