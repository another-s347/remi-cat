//! ChatGPT plan OAuth and the public Responses API. Tokens never enter model YAML.
mod auth;
mod credential;
pub use credential::{
    model_auth_key_from_env, model_auth_key_from_values, resolve_model_credential, ModelCredential,
};
mod responses;
pub use auth::{ChatGptAuthManager, LoginAttempt, LoginCallback};
pub use responses::ChatGptResponsesClient;
pub const API_BASE: &str = "https://api.openai.com/v1";
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
