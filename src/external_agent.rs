use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use a2a::{Message, Part, PartContent, Role, SendMessageRequest, StreamResponse, TaskState};
use anyhow::Context;
use bot_core::{DynamicTool, DynamicToolRisk};
use futures::{future::join_all, stream};
use remi_agentloop::prelude::{AgentError, ToolOutput, ToolResult};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::time::Instant;

use crate::a2a_stdio::{read_frame, write_frame, StdioA2aRequest, StdioA2aResponse};
use crate::instance_profile::{InstanceProfile, ProfileEndpoint};
use crate::profile_hub::ProfileHubClient;
use crate::profile_registry::ProfileRegistry;

const AGENT_CARD_TIMEOUT: Duration = Duration::from_secs(15);
const AGENT_TASK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(crate) struct ExternalAgentClient {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}

impl ExternalAgentClient {
    pub(crate) async fn spawn(
        profile: &InstanceProfile,
        registry_root: &Path,
    ) -> anyhow::Result<Self> {
        let command_line = profile.expanded_local_command()?;
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt;

            let mut command = tokio::process::Command::new("cmd.exe");
            command.args(["/D", "/S", "/C"]);
            command.as_std_mut().raw_arg(format!("\"{command_line}\""));
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", &command_line]);
            command
        };
        command
            .env("REMI_PROFILE_ID", &profile.manifest.id)
            .env("REMI_DATA_DIR", &profile.data_dir)
            .env("REMI_RUNTIME_CONFIG", &profile.runtime_config)
            .env("REMI_AGENTS_DIR", &profile.agents_dir)
            .env("REMI_MODELS_DIR", &profile.models_dir)
            .env("REMI_MEMORY_DIR", &profile.memory_dir)
            .env("REMI_SESSIONS_PATH", &profile.sessions_path)
            .env("REMI_USERS_PATH", &profile.users_path)
            .env("REMI_TASKS_DIR", &profile.tasks_dir)
            .env("REMI_WORKFLOWS_DIR", &profile.workflows_dir)
            .env("REMI_A2A_TRANSPORT", "stdio")
            .env("REMI_PROFILE_REGISTRY_ROOT", registry_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(path) = profile.manifest_path.as_deref() {
            command.env("REMI_PROFILE_PATH", path);
        }
        if let Some(workspace) = profile.workspace.as_deref() {
            command
                .env("REMI_PROFILE_WORKSPACE", workspace)
                .current_dir(workspace);
        } else if let Some(directory) = profile.manifest_path.as_deref().and_then(Path::parent) {
            command.current_dir(directory);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting local external agent `{}`", profile.manifest.id))?;
        let input = child
            .stdin
            .take()
            .context("local external agent stdin was not piped")?;
        let output = child
            .stdout
            .take()
            .context("local external agent stdout was not piped")?;
        Ok(Self {
            child,
            input,
            output,
        })
    }

    pub(crate) async fn agent_card(&mut self) -> anyhow::Result<a2a::AgentCard> {
        let id = uuid::Uuid::new_v4().to_string();
        write_frame(
            &mut self.input,
            &StdioA2aRequest::AgentCard { id: id.clone() },
        )
        .await?;
        let deadline = Instant::now() + AGENT_CARD_TIMEOUT;
        loop {
            match read_frame_until(&mut self.output, deadline, "Agent Card").await? {
                Some(StdioA2aResponse::AgentCard {
                    id: response_id,
                    card,
                }) if response_id == id => return Ok(card),
                Some(StdioA2aResponse::Error {
                    id: response_id,
                    message,
                }) if response_id == id => anyhow::bail!(message),
                Some(_) => continue,
                None => anyhow::bail!("local external agent exited before returning Agent Card"),
            }
        }
    }

    pub(crate) async fn ask(
        &mut self,
        caller_profile_id: &str,
        target_profile_id: &str,
        named: &str,
        task: &str,
        agent_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let card = self.agent_card().await?;
        if !card
            .supported_interfaces
            .iter()
            .any(|interface| interface.protocol_binding == "urn:remi:a2a:binding:stdio-json:v1")
        {
            anyhow::bail!("local external agent does not advertise the Remi stdio A2A binding");
        }
        let context_id = stable_context_id(caller_profile_id, target_profile_id, named, agent_id);
        let invocation = "urn:remi:a2a:invocation-context:v1".to_string();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            invocation.clone(),
            json!({
                "callerProfile": caller_profile_id,
                "targetProfile": target_profile_id,
                "agentId": agent_id,
                "namedSession": named,
                "chain": [caller_profile_id, target_profile_id],
            }),
        );
        let id = uuid::Uuid::new_v4().to_string();
        let request = SendMessageRequest {
            message: Message {
                message_id: uuid::Uuid::new_v4().to_string(),
                context_id: Some(context_id),
                task_id: None,
                role: Role::User,
                parts: vec![Part::text(task)],
                metadata: Some(metadata),
                extensions: Some(vec![invocation]),
                reference_task_ids: None,
            },
            configuration: None,
            metadata: None,
            tenant: None,
        };
        write_frame(
            &mut self.input,
            &StdioA2aRequest::MessageStream {
                id: id.clone(),
                request,
            },
        )
        .await?;

        let mut answer = String::new();
        let mut terminal = None;
        let deadline = Instant::now() + AGENT_TASK_TIMEOUT;
        loop {
            let Some(response) =
                read_frame_until(&mut self.output, deadline, "task completion").await?
            else {
                anyhow::bail!("local external agent exited during A2A task");
            };
            match response {
                StdioA2aResponse::Event {
                    id: response_id,
                    event,
                } if response_id == id => collect_event(event, &mut answer, &mut terminal),
                StdioA2aResponse::Done { id: response_id } if response_id == id => break,
                StdioA2aResponse::Error {
                    id: response_id,
                    message,
                } if response_id == id => anyhow::bail!(message),
                _ => {}
            }
        }
        if let Some(state) = terminal {
            if state != TaskState::Completed {
                anyhow::bail!("external agent task ended in state {state:?}");
            }
        }
        if answer.is_empty() {
            anyhow::bail!("external agent completed without an answer artifact");
        }
        Ok(answer)
    }

    pub(crate) async fn shutdown(mut self) -> anyhow::Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        let _ = write_frame(
            &mut self.input,
            &StdioA2aRequest::Shutdown { id: id.clone() },
        )
        .await;
        drop(self.input);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait())
            .await
            .context("timed out stopping local external agent")??;
        if !status.success() {
            anyhow::bail!("local external agent exited with {status}");
        }
        Ok(())
    }
}

async fn read_frame_until<T: serde::de::DeserializeOwned>(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    deadline: Instant,
    phase: &str,
) -> anyhow::Result<Option<T>> {
    tokio::time::timeout_at(deadline, read_frame(reader))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for local external agent {phase}"))?
}

fn collect_event(event: StreamResponse, answer: &mut String, terminal: &mut Option<TaskState>) {
    match event {
        StreamResponse::ArtifactUpdate(update) => append_parts(&update.artifact.parts, answer),
        StreamResponse::Task(task) => {
            *terminal = Some(task.status.state);
            if answer.is_empty() {
                if let Some(artifacts) = task.artifacts {
                    for artifact in artifacts {
                        append_parts(&artifact.parts, answer);
                    }
                }
            }
        }
        StreamResponse::Message(message) => append_parts(&message.parts, answer),
        StreamResponse::StatusUpdate(update) if update.status.state.is_terminal() => {
            *terminal = Some(update.status.state)
        }
        StreamResponse::StatusUpdate(_) => {}
    }
}

fn append_parts(parts: &[Part], answer: &mut String) {
    for part in parts {
        if let PartContent::Text(text) = &part.content {
            answer.push_str(text);
        }
    }
}

fn stable_context_id(caller: &str, target: &str, named: &str, agent_id: Option<&str>) -> String {
    let digest = Sha256::digest(
        format!(
            "remi:a2a:{caller}:{target}:{}:{named}",
            agent_id.unwrap_or("default")
        )
        .as_bytes(),
    );
    format!("remi-{}", hex_string(&digest))
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) async fn ask_profile(
    registry_root: &Path,
    caller_profile_id: &str,
    reference: &str,
    task: &str,
    named: &str,
    agent_id: Option<&str>,
    profile_hubs: &[ProfileHubClient],
) -> anyhow::Result<String> {
    if let Some(remote) = reference.strip_prefix("hub:") {
        let (hub_id, hub_profile_id) = remote
            .split_once('/')
            .filter(|(hub_id, profile_id)| !hub_id.is_empty() && !profile_id.is_empty())
            .context("invalid Profile Hub reference; expected hub:<hub-id>/<profile-id>")?;
        let hub = profile_hubs
            .iter()
            .find(|hub| hub.id() == hub_id)
            .with_context(|| format!("Profile Hub `{hub_id}` is not configured"))?;
        let context_id = stable_context_id(caller_profile_id, reference, named, agent_id);
        let invocation = "urn:remi:a2a:invocation-context:v1".to_string();
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            invocation.clone(),
            json!({
                "callerProfile": caller_profile_id,
                "targetProfile": reference,
                "agentId": agent_id,
                "namedSession": named,
                "chain": [caller_profile_id, reference],
            }),
        );
        let request = SendMessageRequest {
            message: Message {
                message_id: uuid::Uuid::new_v4().to_string(),
                context_id: Some(context_id),
                task_id: None,
                role: Role::User,
                parts: vec![Part::text(task)],
                metadata: Some(metadata),
                extensions: Some(vec![invocation]),
                reference_task_ids: None,
            },
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let mut answer = String::new();
        let mut terminal = None;
        for event in hub.invoke(hub_profile_id, &request).await? {
            collect_event(event, &mut answer, &mut terminal);
        }
        match terminal {
            Some(TaskState::Completed) => {}
            Some(state) => anyhow::bail!("remote external agent task ended in state {state:?}"),
            None => anyhow::bail!("remote external agent stream ended without terminal state"),
        }
        if answer.is_empty() {
            anyhow::bail!("remote external agent completed without an answer artifact");
        }
        return Ok(answer);
    }
    let registry = ProfileRegistry::load(registry_root)?;
    let profile = registry.resolve(reference)?;
    if profile.manifest.id == caller_profile_id {
        anyhow::bail!("external agent profile `{caller_profile_id}` cannot ask itself");
    }
    match &profile.endpoint {
        ProfileEndpoint::Local { .. } => {
            let mut client = ExternalAgentClient::spawn(&profile, registry_root).await?;
            let result = client
                .ask(
                    caller_profile_id,
                    &profile.manifest.id,
                    named,
                    task,
                    agent_id,
                )
                .await;
            let shutdown = client.shutdown().await;
            match (result, shutdown) {
                (Ok(answer), Ok(())) => Ok(answer),
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
            }
        }
        ProfileEndpoint::Remote { .. } => anyhow::bail!(
            "REMOTE_AGENT_NOT_IMPLEMENTED: remote profile `{}` is declared but remote transport is not implemented",
            profile.manifest.id
        ),
    }
}

pub(crate) fn external_agent_tools(
    registry_root: PathBuf,
    profile_hubs: Vec<ProfileHubClient>,
) -> Vec<DynamicTool> {
    let caller_profile_id =
        std::env::var("REMI_PROFILE_ID").unwrap_or_else(|_| "remi.default".to_string());
    external_agent_tools_for(registry_root, caller_profile_id, profile_hubs)
}

pub(crate) fn external_agent_tools_for(
    registry_root: PathBuf,
    caller_profile_id: String,
    profile_hubs: Vec<ProfileHubClient>,
) -> Vec<DynamicTool> {
    let discover_root = registry_root.clone();
    let discover_caller_profile_id = caller_profile_id.clone();
    let discover_profile_hubs = profile_hubs.clone();
    let discover = DynamicTool::from_parts(
        "external_agent_discover",
        "Discover local registered profiles and online remote profiles from the configured Profile Hub by declared tags and intents.",
        json!({
            "type": "object",
            "properties": {
                "tags": {"type": "array", "items": {"type": "string"}},
                "intents": {"type": "array", "items": {"type": "string"}}
            },
            "additionalProperties": false
        }),
        move |arguments, _resume, _ctx| {
            let registry_root = discover_root.clone();
            let caller_profile_id = discover_caller_profile_id.clone();
            let profile_hubs = discover_profile_hubs.clone();
            async move {
                let tags = string_array(&arguments, "tags");
                let intents = string_array(&arguments, "intents");
                let matches = discover_profiles(
                    &registry_root,
                    &caller_profile_id,
                    &tags,
                    &intents,
                    &profile_hubs,
                )
                .await
                .map_err(|error| AgentError::tool("external-agent", error.to_string()))?;
                let text = serde_json::to_string_pretty(&matches)
                    .map_err(|error| AgentError::tool("external-agent", error.to_string()))?;
                Ok::<_, AgentError>(ToolResult::Output(stream::iter(vec![ToolOutput::text(text)])))
            }
        },
    )
    .risk(DynamicToolRisk::Low);

    let ask = DynamicTool::from_parts(
        "external_agent_ask",
        "Communicate with a discovered local or Profile Hub external agent over A2A.",
        json!({
            "type": "object",
            "properties": {
                "profile": {"type": "string", "description": "A local registered profile such as @travel or a discovered hub:<hub-id>/<profile-id> reference."},
                "task": {"type": "string"},
                "named": {"type": "string", "default": "default"},
                "agent_id": {"type": "string"}
            },
            "required": ["profile", "task"],
            "additionalProperties": false
        }),
        move |arguments, _resume, _ctx| {
            let registry_root = registry_root.clone();
            let caller_profile_id = caller_profile_id.clone();
            let profile_hubs = profile_hubs.clone();
            async move {
                let reference = required_string(&arguments, "profile")?;
                let task = required_string(&arguments, "task")?;
                let named = arguments
                    .get("named")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("default")
                    .to_string();
                let agent_id = arguments
                    .get("agent_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                let answer = ask_profile(
                    &registry_root,
                    &caller_profile_id,
                    &reference,
                    &task,
                    &named,
                    agent_id.as_deref(),
                    &profile_hubs,
                )
                .await
                .map_err(|error| AgentError::tool("external-agent", error.to_string()))?;
                Ok::<_, AgentError>(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                    answer,
                )])))
            }
        },
    )
    // Registry membership is the local trust boundary: registered profile
    // endpoints are executable configuration, analogous to local delegates.
    .risk(DynamicToolRisk::Low);
    vec![discover, ask]
}

async fn discover_profiles(
    registry_root: &Path,
    caller_profile_id: &str,
    tags: &[String],
    intents: &[String],
    profile_hubs: &[ProfileHubClient],
) -> anyhow::Result<Vec<serde_json::Value>> {
    let registry = ProfileRegistry::load(registry_root)?;
    let mut matches = Vec::new();
    for entry in registry.entries() {
        let Ok(profile) = InstanceProfile::from_manifest(&entry.manifest_path) else {
            continue;
        };
        if profile.manifest.id == caller_profile_id {
            continue;
        }
        if !tags
            .iter()
            .all(|tag| profile.manifest.capabilities.tags.contains(tag))
            || !intents
                .iter()
                .all(|intent| profile.manifest.capabilities.intents.contains(intent))
        {
            continue;
        }
        matches.push(json!({
            "reference": format!("@{}", entry.alias),
            "source": "local_registry",
            "id": profile.manifest.id,
            "name": profile.manifest.name,
            "description": profile.manifest.description,
            "endpoint_type": match profile.endpoint { ProfileEndpoint::Local { .. } => "local", ProfileEndpoint::Remote { .. } => "remote" },
            "status": "available",
            "tags": profile.manifest.capabilities.tags,
            "intents": profile.manifest.capabilities.intents,
        }));
    }
    let remote_results = join_all(
        profile_hubs
            .iter()
            .map(|hub| async move { (hub.id().to_string(), hub.discover(tags, intents).await) }),
    )
    .await;
    let mut successful_hubs = 0usize;
    let mut failures = Vec::new();
    for (hub_id, result) in remote_results {
        match result {
            Ok(remote) => {
                successful_hubs += 1;
                matches.extend(remote.iter().map(|profile| profile.tool_value(&hub_id)));
            }
            Err(error) => failures.push((hub_id, error)),
        }
    }
    if successful_hubs == 0 && !profile_hubs.is_empty() {
        let details = failures
            .iter()
            .map(|(hub_id, error)| format!("{hub_id}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("all configured Profile Hubs failed: {details}");
    }
    for (hub_id, error) in failures {
        tracing::warn!(hub_id, error = %error, "Profile Hub discovery failed; returning results from other sources");
    }
    Ok(matches)
}

fn string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect()
}

fn required_string(value: &serde_json::Value, key: &str) -> Result<String, AgentError> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AgentError::tool("external-agent", format!("missing `{key}`")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::{Json, Router};
    use remi_agentloop::prelude::Tool;

    #[cfg(windows)]
    use tokio::io::AsyncReadExt;

    #[test]
    fn external_tool_names_are_explicit_and_stable() {
        let tools = external_agent_tools(PathBuf::from("registry"), Vec::new());
        let names = tools
            .iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, ["external_agent_discover", "external_agent_ask"]);
        assert!(names.iter().all(|name| !name.starts_with("agent_")));
        assert!(tools
            .iter()
            .all(|tool| tool.declared_risk() == Some(DynamicToolRisk::Low)));
    }

    #[test]
    fn named_conversations_have_stable_distinct_contexts() {
        assert_eq!(
            stable_context_id("a", "b", "summer", None),
            stable_context_id("a", "b", "summer", None)
        );
        assert_ne!(
            stable_context_id("a", "b", "summer", None),
            stable_context_id("a", "b", "winter", None)
        );
        assert_ne!(
            stable_context_id("a", "b", "summer", Some("planner")),
            stable_context_id("a", "b", "summer", Some("accounting"))
        );
    }

    #[tokio::test]
    async fn discovery_combines_local_registry_and_logical_hub() {
        let registry_root = tempfile::tempdir().unwrap();
        let profile_root = tempfile::tempdir().unwrap();
        let manifest = profile_root.path().join("profile.yaml");
        std::fs::write(
            &manifest,
            "schema_version: 1\nid: local.travel\nname: Local Travel\ncapabilities:\n  tags: [travel]\n  intents: [plan-trip]\nendpoint:\n  type: local\n  command: exit 0\n",
        )
        .unwrap();
        ProfileRegistry::load(registry_root.path())
            .unwrap()
            .register(&manifest, Some("local-travel"), false)
            .unwrap();

        let app = Router::new().route(
            "/api/v1/profiles",
            get(|| async {
                Json(json!({"items":[{
                    "hub_profile_id":"remote-1",
                    "device_id":"device-2",
                    "profile_id":"remote.travel",
                    "alias":"travel",
                    "name":"Remote Travel",
                    "description":null,
                    "version":null,
                    "tags":["travel"],
                    "intents":["plan-trip"],
                    "status":"online"
                }]}))
            }),
        );
        let office_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let office_address = office_listener.local_addr().unwrap();
        let home_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let home_address = home_listener.local_addr().unwrap();
        let unavailable_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_address = unavailable_listener.local_addr().unwrap();
        drop(unavailable_listener);
        let home_app = app.clone();
        tokio::spawn(async move { axum::serve(office_listener, app).await.unwrap() });
        tokio::spawn(async move { axum::serve(home_listener, home_app).await.unwrap() });
        let office =
            ProfileHubClient::for_test("office", format!("http://{office_address}"), "token");
        let home = ProfileHubClient::for_test("home", format!("http://{home_address}"), "token");
        let unavailable =
            ProfileHubClient::for_test("offline", format!("http://{unavailable_address}"), "token");

        let found = discover_profiles(
            registry_root.path(),
            "caller",
            &["travel".to_string()],
            &["plan-trip".to_string()],
            &[office, home, unavailable],
        )
        .await
        .unwrap();

        assert_eq!(found.len(), 3);
        assert_eq!(found[0]["reference"], "@local-travel");
        assert_eq!(found[0]["source"], "local_registry");
        assert_eq!(found[1]["reference"], "hub:office/remote-1");
        assert_eq!(found[1]["source"], "profile_hub");
        assert_eq!(found[2]["reference"], "hub:home/remote-1");
        assert_eq!(found[2]["hub_id"], "home");
    }

    #[tokio::test]
    async fn hub_reference_requires_configured_hub() {
        let registry_root = tempfile::tempdir().unwrap();
        let error = ask_profile(
            registry_root.path(),
            "caller",
            "hub:office/remote-1",
            "delegate this",
            "default",
            None,
            &[],
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("Profile Hub `office` is not configured"));
    }

    #[tokio::test]
    async fn live_profile_hubs_from_env() {
        let Ok(config_path) = std::env::var("REMI_PROFILE_HUB_E2E_RUNTIME_CONFIG") else {
            return;
        };
        let expected = std::env::var("REMI_PROFILE_HUB_E2E_EXPECT_REFERENCES")
            .unwrap_or_default()
            .split(',')
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let config_path = std::path::PathBuf::from(config_path);
        let data_dir = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let config = crate::runtime_config::load_runtime_config_at(&config_path, data_dir)
            .unwrap()
            .expect("E2E runtime config exists");
        let clients = crate::profile_hub::clients_from_runtime(&config, &config_path).unwrap();
        let registry_root = tempfile::tempdir().unwrap();
        let profile_root = tempfile::tempdir().unwrap();
        let manifest = profile_root.path().join("profile.yaml");
        std::fs::write(
            &manifest,
            "schema_version: 1\nid: local.e2e\nname: Local E2E\ncapabilities:\n  tags: [e2e]\n  intents: [verify]\nendpoint:\n  type: local\n  command: exit 0\n",
        )
        .unwrap();
        ProfileRegistry::load(registry_root.path())
            .unwrap()
            .register(&manifest, Some("local-e2e"), false)
            .unwrap();

        let found = discover_profiles(
            registry_root.path(),
            "caller",
            &["e2e".to_string()],
            &["verify".to_string()],
            &clients,
        )
        .await
        .unwrap();
        let references = found
            .iter()
            .filter_map(|item| item["reference"].as_str())
            .collect::<Vec<_>>();
        assert!(references.contains(&"@local-e2e"));
        for reference in &expected {
            assert!(references.contains(&reference.as_str()), "{references:?}");
        }
        assert_eq!(references.len(), expected.len() + 1, "{references:?}");
    }

    #[tokio::test]
    async fn framed_reads_honor_the_overall_deadline() {
        let (_writer, mut reader) = tokio::io::duplex(64);
        let error = read_frame_until::<StdioA2aResponse>(
            &mut reader,
            Instant::now() + Duration::from_millis(20),
            "test response",
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("timed out waiting for local external agent test response"));
    }

    #[tokio::test]
    async fn explicit_ask_rejects_the_caller_profile_before_starting_it() {
        let registry_root = tempfile::tempdir().unwrap();
        let profile_root = tempfile::tempdir().unwrap();
        let manifest = profile_root.path().join("profile.yaml");
        std::fs::write(
            &manifest,
            "schema_version: 1\nid: embedded.ferret\nname: Ferret\nendpoint:\n  type: local\n  command: exit 99\n",
        )
        .unwrap();
        ProfileRegistry::load(registry_root.path())
            .unwrap()
            .register(&manifest, Some("ferret"), false)
            .unwrap();

        let error = ask_profile(
            registry_root.path(),
            "embedded.ferret",
            "@ferret",
            "delegate this",
            "default",
            None,
            &[],
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("cannot ask itself"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn quoted_local_endpoint_with_spaces_is_passed_raw_to_cmd() {
        let root = tempfile::tempdir().unwrap();
        let endpoint_dir = root.path().join("endpoint with spaces");
        std::fs::create_dir_all(&endpoint_dir).unwrap();
        let endpoint = endpoint_dir.join("fixture.cmd");
        std::fs::write(&endpoint, "@echo off\r\necho RAW_ARG_OK\r\n").unwrap();
        let manifest = root.path().join("profile.yaml");
        std::fs::write(
            &manifest,
            format!(
                "schema_version: 1\nid: raw-arg-test\nname: Raw Arg Test\nendpoint:\n  type: local\n  command: '\"{}\"'\n",
                endpoint.display().to_string().replace('\'', "''")
            ),
        )
        .unwrap();
        let profile = InstanceProfile::from_manifest(&manifest).unwrap();
        let ExternalAgentClient {
            mut child,
            input,
            mut output,
        } = ExternalAgentClient::spawn(&profile, root.path())
            .await
            .unwrap();
        drop(input);
        let mut text = String::new();
        output.read_to_string(&mut text).await.unwrap();
        let status = child.wait().await.unwrap();

        assert!(status.success());
        assert_eq!(text.trim(), "RAW_ARG_OK");
    }
}
