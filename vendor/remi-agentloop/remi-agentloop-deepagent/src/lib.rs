//! `remi-deepagent` — a production-ready deep agent with:
//!
//! - **Todo layer** (`todo__*` tools) — manage a todo list persisted in agent state
//! - **Skill layer** (`skill__*` tools) — save and recall reusable procedures as `.md` files
//! - **Context compression** — automatically summarise long histories before forwarding
//! - **File-backed registry** — spill oversized tool outputs to disk instead of flooding context
//! - **Sub-agent task tool** (`task__run`) — delegate focused subtasks to a worker agent
//! - **Bash** + **filesystem** tools built in
//!
//! # Quick start
//!
//! ```no_run
//! use remi_deepagent::DeepAgentBuilder;
//! use remi_model::OpenAIClient;
//! use futures::StreamExt;
//!
//! #[tokio::main]
//! async fn main() {
//!     let model = OpenAIClient::new(std::env::var("OPENAI_API_KEY").unwrap());
//!     let agent = DeepAgentBuilder::new(model).build();
//!     let mut stream = agent.chat("Create a hello-world Rust project in /tmp/demo").await.unwrap();
//!     while let Some(ev) = stream.next().await {
//!         println!("{ev:?}");
//!     }
//! }
//! ```

pub mod agent;
pub mod compress;
pub mod config_file;
pub mod events;
pub mod registry;
pub mod search;
pub mod skill;
pub mod sub_agent;
pub mod task;
pub mod todo;
pub mod workspace_fs;

// ── Top-level re-exports ───────────────────────────────────────────────────────

pub use agent::{DeepAgent, DeepAgentBuilder};
pub use compress::CompressingLayer;
pub use config_file::DeepAgentConfig;
pub use events::{DeepAgentEvent, SkillEvent, TodoEvent};
pub use registry::FileBackedRegistry;
pub use search::{ExaSearchTool, TavilySearchTool};
#[cfg(feature = "skill-virtual")]
pub use skill::store::FsSkillStore;
pub use skill::{
    store::{FileSkillStore, InMemorySkillStore, SkillStore},
    SkillLayer,
};
pub use sub_agent::{SubAgentEventStream, SubAgentToolAdapter};
pub use task::SubAgentTaskTool;
pub use todo::{tools::TodoToolkit, TodoLayer};
pub use workspace_fs::{
    RootedFsCreateTool, RootedFsLsTool, RootedFsReadTool, RootedFsRemoveTool, RootedFsWriteTool,
    WorkspaceBashTool,
};
