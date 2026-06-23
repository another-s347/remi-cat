//! Command execution boundary.
//!
//! Setup, doctor, secrets, update, feedback, hooks, and runtime slash commands
//! should be moved here as the `app` module is reduced to startup orchestration.

pub(crate) use crate::app::{process_runtime_commands, RuntimeCommandPipelineResult};
