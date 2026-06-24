//! LLM provider and model-profile building blocks.
//!
//! This module is the public composition surface for code that wants to build
//! a remi-cat runtime with a specific model/provider profile.

pub use crate::model_profile::{
    api_key_from_env, install_embedded_model_profiles, resolve_model_profile_from_env,
    ModelProfileConfig, ModelProfileRegistry, ModelProfileSource, ResolvedModelProfile,
    ThinkingMode,
};
pub use crate::model_usage::{AccountBalance, AccountUsage, AccountUsageStatus};
