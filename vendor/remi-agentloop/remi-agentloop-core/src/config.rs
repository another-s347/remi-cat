use crate::error::AgentError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

mod rate_limit_retry_defaults {
    pub const fn max_retries() -> usize {
        4
    }
    pub const fn initial_delay_ms() -> u64 {
        500
    }
    pub const fn max_delay_ms() -> u64 {
        8_000
    }
    pub const fn multiplier() -> f64 {
        2.0
    }
    pub const fn respect_retry_after() -> bool {
        true
    }
}

/// Backoff policy for retrying model calls that fail with HTTP 429.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitRetryPolicy {
    #[serde(default = "rate_limit_retry_defaults::max_retries")]
    pub max_retries: usize,

    #[serde(default = "rate_limit_retry_defaults::initial_delay_ms")]
    pub initial_delay_ms: u64,

    #[serde(default = "rate_limit_retry_defaults::max_delay_ms")]
    pub max_delay_ms: u64,

    #[serde(default = "rate_limit_retry_defaults::multiplier")]
    pub multiplier: f64,

    #[serde(default = "rate_limit_retry_defaults::respect_retry_after")]
    pub respect_retry_after: bool,
}

impl Default for RateLimitRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: rate_limit_retry_defaults::max_retries(),
            initial_delay_ms: rate_limit_retry_defaults::initial_delay_ms(),
            max_delay_ms: rate_limit_retry_defaults::max_delay_ms(),
            multiplier: rate_limit_retry_defaults::multiplier(),
            respect_retry_after: rate_limit_retry_defaults::respect_retry_after(),
        }
    }
}

impl RateLimitRetryPolicy {
    pub fn delay_for_retry(&self, retry_index: usize, server_hint: Option<Duration>) -> Duration {
        let computed = self.exponential_delay(retry_index);
        if self.respect_retry_after {
            if let Some(server_hint) = server_hint {
                return server_hint.max(computed);
            }
        }
        computed
    }

    pub fn exponential_delay(&self, retry_index: usize) -> Duration {
        let multiplier = if self.multiplier < 1.0 {
            1.0
        } else {
            self.multiplier
        };
        let delay_ms = (self.initial_delay_ms as f64) * multiplier.powi(retry_index as i32);
        let capped = delay_ms.min(self.max_delay_ms as f64).max(0.0);
        Duration::from_millis(capped.round() as u64)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn from_env() -> Option<Self> {
        let max_retries = std::env::var("REMI_RATE_LIMIT_MAX_RETRIES").ok();
        let initial_delay_ms = std::env::var("REMI_RATE_LIMIT_INITIAL_DELAY_MS").ok();
        let max_delay_ms = std::env::var("REMI_RATE_LIMIT_MAX_DELAY_MS").ok();
        let multiplier = std::env::var("REMI_RATE_LIMIT_MULTIPLIER").ok();
        let respect_retry_after = std::env::var("REMI_RATE_LIMIT_RESPECT_RETRY_AFTER").ok();

        if max_retries.is_none()
            && initial_delay_ms.is_none()
            && max_delay_ms.is_none()
            && multiplier.is_none()
            && respect_retry_after.is_none()
        {
            return None;
        }

        let mut policy = Self::default();
        if let Some(value) = max_retries.and_then(|value| value.parse().ok()) {
            policy.max_retries = value;
        }
        if let Some(value) = initial_delay_ms.and_then(|value| value.parse().ok()) {
            policy.initial_delay_ms = value;
        }
        if let Some(value) = max_delay_ms.and_then(|value| value.parse().ok()) {
            policy.max_delay_ms = value;
        }
        if let Some(value) = multiplier.and_then(|value| value.parse().ok()) {
            policy.multiplier = value;
        }
        if let Some(value) = respect_retry_after.and_then(|value| value.parse().ok()) {
            policy.respect_retry_after = value;
        }
        Some(policy)
    }
}

/// Runtime configuration for an agent invocation.
///
/// All fields are `Option` so that this struct can be serialised across
/// WASM guest/host boundaries: unset fields indicate “use the implementation
/// default”.  Build a config using the fluent setter methods or load from
/// environment variables with [`AgentConfig::from_env`].
///
/// # Example
///
/// ```ignore
/// use remi_agentloop_core::config::AgentConfig;
///
/// // Programmatic construction
/// let config = AgentConfig::new()
///     .with_api_key("sk-...")
///     .with_model("gpt-4o")
///     .with_temperature(0.7)
///     .with_max_tokens(2048);
///
/// // Load from REMI_* environment variables
/// let config = AgentConfig::from_env();
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_retry: Option<RateLimitRetryPolicy>,

    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub extra: serde_json::Value,
}

impl AgentConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }
    pub fn with_temperature(mut self, temp: f64) -> Self {
        self.temperature = Some(temp);
        self
    }
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }
    pub fn with_rate_limit_retry(mut self, policy: RateLimitRetryPolicy) -> Self {
        self.rate_limit_retry = Some(policy);
        self
    }
    pub fn with_extra(mut self, extra: serde_json::Value) -> Self {
        self.extra = extra;
        self
    }

    /// Load from REMI_* environment variables
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_env() -> Self {
        Self {
            api_key: std::env::var("REMI_API_KEY").ok(),
            model: std::env::var("REMI_MODEL").ok(),
            base_url: std::env::var("REMI_BASE_URL").ok(),
            temperature: std::env::var("REMI_TEMPERATURE")
                .ok()
                .and_then(|s| s.parse().ok()),
            max_tokens: std::env::var("REMI_MAX_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok()),
            timeout_ms: std::env::var("REMI_TIMEOUT_MS")
                .ok()
                .and_then(|s| s.parse().ok()),
            rate_limit_retry: RateLimitRetryPolicy::from_env(),
            ..Default::default()
        }
    }

    /// Merge: fields from `other` override `self` when Some
    pub fn merge(mut self, other: &AgentConfig) -> Self {
        if other.api_key.is_some() {
            self.api_key = other.api_key.clone();
        }
        if other.model.is_some() {
            self.model = other.model.clone();
        }
        if other.base_url.is_some() {
            self.base_url = other.base_url.clone();
        }
        if other.temperature.is_some() {
            self.temperature = other.temperature;
        }
        if other.max_tokens.is_some() {
            self.max_tokens = other.max_tokens;
        }
        if other.timeout_ms.is_some() {
            self.timeout_ms = other.timeout_ms;
        }
        if other.rate_limit_retry.is_some() {
            self.rate_limit_retry = other.rate_limit_retry.clone();
        }
        for (k, v) in &other.headers {
            self.headers.insert(k.clone(), v.clone());
        }
        if !other.extra.is_null() {
            self.extra = other.extra.clone();
        }
        self
    }
}

/// Dynamic config provider — called on each chat() invocation
pub trait ConfigProvider {
    fn resolve(&self) -> impl Future<Output = Result<AgentConfig, AgentError>>;
}

impl ConfigProvider for AgentConfig {
    fn resolve(&self) -> impl Future<Output = Result<AgentConfig, AgentError>> {
        let cfg = self.clone();
        async move { Ok(cfg) }
    }
}
