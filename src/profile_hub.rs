use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::runtime_config::ProfileHubConfig;

#[derive(Clone)]
pub(crate) struct ProfileHubClient {
    client: reqwest::Client,
    id: String,
    base_url: String,
    token: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct RemoteProfile {
    pub hub_profile_id: String,
    pub device_id: String,
    pub profile_id: String,
    pub alias: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub intents: Vec<String>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct ListResponse<T> {
    items: Vec<T>,
}

impl ProfileHubClient {
    pub(crate) fn from_config(config: &ProfileHubConfig) -> Result<Option<Self>> {
        config.validate()?;
        if !config.enabled {
            return Ok(None);
        }
        let token = std::env::var(&config.token_env).with_context(|| {
            format!(
                "Profile Hub is enabled but secret `{}` is unavailable",
                config.token_env
            )
        })?;
        if token.trim().is_empty() {
            anyhow::bail!(
                "Profile Hub is enabled but secret `{}` is empty",
                config.token_env
            );
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(config.request_timeout_ms))
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .context("building Profile Hub client")?;
        Ok(Some(Self {
            client,
            id: config.id.clone(),
            base_url: config.url.trim_end_matches('/').to_string(),
            token,
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        id: impl Into<String>,
        base_url: String,
        token: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            id: id.into(),
            base_url,
            token: token.into(),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) async fn discover(
        &self,
        tags: &[String],
        intents: &[String],
    ) -> Result<Vec<RemoteProfile>> {
        let mut request = self
            .client
            .get(format!("{}/api/v1/profiles", self.base_url))
            .bearer_auth(&self.token)
            .query(&[("status", "online")]);
        let tags_value = (!tags.is_empty()).then(|| tags.join(","));
        let intents_value = (!intents.is_empty()).then(|| intents.join(","));
        if let Some(tags) = tags_value.as_deref() {
            request = request.query(&[("tags", tags)]);
        }
        if let Some(intents) = intents_value.as_deref() {
            request = request.query(&[("intents", intents)]);
        }
        let response = request.send().await.context("querying Profile Hub")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Profile Hub discovery returned HTTP {status}: {}",
                body.trim()
            );
        }
        Ok(response
            .json::<ListResponse<RemoteProfile>>()
            .await
            .context("decoding Profile Hub discovery response")?
            .items)
    }
}

impl RemoteProfile {
    pub(crate) fn reference(&self, hub_id: &str) -> String {
        format!("hub:{hub_id}/{}", self.hub_profile_id)
    }

    pub(crate) fn tool_value(&self, hub_id: &str) -> Value {
        json!({
            "reference": self.reference(hub_id),
            "source": "profile_hub",
            "hub_id": hub_id,
            "hub_profile_id": self.hub_profile_id,
            "device_id": self.device_id,
            "id": self.profile_id,
            "alias": self.alias,
            "name": self.name,
            "description": self.description,
            "version": self.version,
            "endpoint_type": "remote",
            "status": self.status,
            "tags": self.tags,
            "intents": self.intents,
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::extract::Query;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::{json, Value};

    use super::*;

    #[tokio::test]
    async fn discovers_online_profiles_with_auth_and_filters() {
        async fn list(headers: HeaderMap, Query(query): Query<Value>) -> (StatusCode, Json<Value>) {
            if headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                != Some("Bearer hub-token")
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error":"unauthorized"})),
                );
            }
            assert_eq!(query["status"], "online");
            assert_eq!(query["tags"], "travel,tools");
            assert_eq!(query["intents"], "plan-trip");
            (
                StatusCode::OK,
                Json(json!({"items":[{
                    "hub_profile_id":"hub-profile-1",
                    "device_id":"device-1",
                    "profile_id":"travel",
                    "alias":"travel",
                    "name":"Travel Planner",
                    "description":"Plans trips",
                    "version":"1.0.0",
                    "tags":["travel","tools"],
                    "intents":["plan-trip"],
                    "status":"online"
                }]})),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api/v1/profiles", get(list)))
                .await
                .unwrap();
        });
        let client = ProfileHubClient::for_test("office", format!("http://{address}"), "hub-token");
        let profiles = client
            .discover(
                &["travel".to_string(), "tools".to_string()],
                &["plan-trip".to_string()],
            )
            .await
            .unwrap();

        assert_eq!(profiles.len(), 1);
        assert_eq!(
            profiles[0].reference(client.id()),
            "hub:office/hub-profile-1"
        );
        assert_eq!(profiles[0].tool_value(client.id())["source"], "profile_hub");
    }

    #[tokio::test]
    async fn rejects_invalid_hub_token() {
        let app = Router::new().route(
            "/api/v1/profiles",
            get(|| async { (StatusCode::UNAUTHORIZED, "no") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = ProfileHubClient::for_test("office", format!("http://{address}"), "wrong")
            .discover(&[], &[])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("HTTP 401"));
    }
}
