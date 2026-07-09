use std::collections::HashSet;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;

use anyhow::Context;

use bot_core::{
    install_embedded_agent_profiles, install_embedded_model_profiles,
    validate_model_profile_api_key, AgentProfile, AgentRegistry, ModelProfileRegistry,
    WorkflowDefinition,
};

use crate::instance_profile::{
    configured_profiles_excluding_in_data_root, discover_profiles_in_data_root, read_run_metadata,
    remove_named_profile_in_data_root, remove_run_metadata, write_run_metadata, InstanceProfile,
    ProfileRunMetadata, DIAGNOSTIC_PROFILE_NAME,
};
use crate::runtime_config::{
    detect_setup_state, write_runtime_config, AcpClient, AcpMode, FeishuTransport, ImMode,
    RuntimeConfig, RuntimeSandboxKind, SetupState, ShellMode,
};

pub(crate) const PROFILE_RUNTIME_ENV_KEYS: &[&str] = &[
    "REMI_DATA_DIR",
    "REMI_PROFILE",
    "REMI_AGENT_ID",
    "REMI_MODEL_PROFILE",
    "REMI_AGENTS_DIR",
    "AGENT_MD_PATH",
    "REMI_SANDBOX_KIND",
    "REMI_SANDBOX_HOST_DIR",
    "REMI_SANDBOX_CONTAINER_DIR",
    "REMI_SANDBOX_IMAGE",
    "REMI_SANDBOX_CONTAINER_NAME",
    "REMI_SANDBOX_USER",
    "REMI_ADMIN_ENABLED",
    "REMI_ADMIN_HOST",
    "REMI_ADMIN_PORT",
    "REMI_IM_MODE",
    "REMI_FEISHU_TRANSPORT",
    "REMI_FEISHU_HOOK_HOST",
    "REMI_FEISHU_HOOK_PORT",
    "REMI_FEISHU_HOOK_PATH",
    "REMI_FEISHU_HOOK_VERIFICATION_TOKEN",
    "REMI_SHELL_MODE",
    "REMI_BASH_MODE",
    "REMI_ACP_MODE",
    "REMI_ACP_CLIENT",
    "REMI_ACP_BASE_URL",
    "REMI_ACP_AGENT_NAME",
    "REMI_ACP_MODEL",
];

#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileCommand {
    List,
    Show(String),
    Create { name: String, entries: Vec<String> },
    Delete { name: String, force: bool },
    Start(String),
    Stop { name: String, force: bool },
    Restart { name: String, force: bool },
    Status(String),
    StatusAll,
    Agent(ProfileAgentCommand),
    Workflow(ProfileWorkflowCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileAgentCommand {
    List { profile: String },
    Show { profile: String, agent_id: String },
    Upsert { profile: String, path: String },
    SetDefault { profile: String, agent_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileWorkflowCommand {
    List {
        profile: String,
    },
    Show {
        profile: String,
        workflow_id: String,
    },
    Upsert {
        profile: String,
        path: String,
    },
    Delete {
        profile: String,
        workflow_id: String,
    },
}

pub async fn run_noninteractive_setup(
    profile: &InstanceProfile,
    data_dir: &Path,
    entries: &[String],
) -> anyhow::Result<()> {
    let entries = entries
        .iter()
        .filter(|entry| entry.as_str() != "--non-interactive")
        .cloned()
        .collect::<Vec<_>>();
    apply_runtime_config_entries(profile, data_dir, &entries, true).await
}

pub async fn run_profile_command(command: &ProfileCommand, data_root: &Path) -> anyhow::Result<()> {
    ensure_builtin_diagnostic_profile_in_data_root(data_root)?;
    match command {
        ProfileCommand::List => {
            println!("NAME\tSETUP\tRUNNING\tADMIN\tSANDBOX\tDATA DIR");
            for profile in discover_profiles_in_data_root(data_root)? {
                let state = detect_setup_state(&profile.data_dir);
                let running = profile_run_state(&profile).label();
                let (setup, admin, sandbox) = match &state {
                    SetupState::Initialized { config, .. } => (
                        "initialized",
                        format_admin_addr(config),
                        config.sandbox.kind.as_env_value().to_string(),
                    ),
                    SetupState::Invalid { .. } => ("invalid", "-".to_string(), "-".to_string()),
                    SetupState::LegacyEnvCompatible { .. } => {
                        ("legacy-env", "-".to_string(), "-".to_string())
                    }
                    SetupState::Uninitialized { .. } => {
                        ("uninitialized", "-".to_string(), "-".to_string())
                    }
                };
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    profile.label(),
                    setup,
                    running,
                    admin,
                    sandbox,
                    profile.data_dir.display()
                );
            }
        }
        ProfileCommand::Show(name) => {
            let profile = profile_from_label(name, data_root)?;
            println!("profile: {}", profile.label());
            println!("data_dir: {}", profile.data_dir.display());
            println!("run_status: {}", profile_run_state(&profile).label());
            match detect_setup_state(&profile.data_dir) {
                SetupState::Initialized {
                    config_path,
                    config,
                } => {
                    println!("status: initialized");
                    println!("runtime_config: {}", config_path.display());
                    println!("root_agent_id: {}", config.root_agent_id);
                    println!("model_profile: {}", config.model_profile);
                    println!(
                        "tool_output_overflow_bytes: {}",
                        config
                            .tool_output
                            .overflow_bytes
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "model_profile_default".to_string())
                    );
                    println!(
                        "tool_foreground_timeout_ms: {}",
                        config
                            .tool_output
                            .foreground_timeout_ms
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "10000".to_string())
                    );
                    println!("async_agent: {}", config.tool_output.async_agent);
                    println!("sandbox_kind: {}", config.sandbox.kind.as_env_value());
                    println!("sandbox_container: {}", config.sandbox.container_name);
                    println!("im_mode: {}", config.im.mode.as_env_value());
                    println!("admin: {}", format_admin_addr(&config));
                    if matches!(config.im.transport, FeishuTransport::EventHook) {
                        println!(
                            "feishu_event_hook: http://{}:{}{}",
                            config.im.event_hook.host,
                            config.im.event_hook.port,
                            config.im.event_hook.path
                        );
                    }
                }
                SetupState::Invalid { config_path, error } => {
                    println!("status: invalid");
                    println!("runtime_config: {}", config_path.display());
                    println!("error: {error}");
                }
                SetupState::LegacyEnvCompatible { .. } => println!("status: legacy-env"),
                SetupState::Uninitialized { .. } => println!("status: uninitialized"),
            }
        }
        ProfileCommand::Create { name, entries } => {
            let profile = InstanceProfile::named_in_data_root(name, data_root)?;
            if matches!(
                detect_setup_state(&profile.data_dir),
                SetupState::Initialized { .. }
            ) {
                anyhow::bail!("profile `{name}` already exists");
            }
            apply_runtime_config_entries(&profile, &profile.data_dir, entries, true).await?;
        }
        ProfileCommand::Delete { name, force } => {
            if !force {
                anyhow::bail!("refusing to delete profile `{name}` without --force");
            }
            let path = remove_named_profile_in_data_root(name, data_root)?;
            println!("Deleted profile `{name}` at {}", path.display());
        }
        ProfileCommand::Start(name) => {
            let profile = profile_from_label(name, data_root)?;
            start_profile(&profile)?;
        }
        ProfileCommand::Stop { name, force } => {
            let profile = profile_from_label(name, data_root)?;
            stop_profile(&profile, *force)?;
        }
        ProfileCommand::Restart { name, force } => {
            let profile = profile_from_label(name, data_root)?;
            stop_profile(&profile, *force)?;
            start_profile(&profile)?;
        }
        ProfileCommand::Status(name) => {
            let profile = profile_from_label(name, data_root)?;
            print_profile_status(&profile)?;
        }
        ProfileCommand::StatusAll => {
            for profile in discover_profiles_in_data_root(data_root)? {
                print_profile_status(&profile)?;
            }
        }
        ProfileCommand::Agent(command) => run_profile_agent_command(command, data_root).await?,
        ProfileCommand::Workflow(command) => run_profile_workflow_command(command, data_root)?,
    }
    Ok(())
}

fn profile_from_label(label: &str, data_root: &Path) -> anyhow::Result<InstanceProfile> {
    InstanceProfile::from_label_in_data_root(label, data_root)
}

async fn run_profile_agent_command(
    command: &ProfileAgentCommand,
    data_root: &Path,
) -> anyhow::Result<()> {
    match command {
        ProfileAgentCommand::List { profile } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile.data_dir)?;
            let registry = AgentRegistry::load(profile.data_dir.join("agents"))?;
            println!("ID\tNAME\tMODEL\tTOOLS\tDESCRIPTION");
            let mut agents = registry.profiles().collect::<Vec<_>>();
            agents.sort_by(|a, b| a.id.cmp(&b.id));
            for agent in agents {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    agent.id,
                    agent.name,
                    agent.model.as_deref().unwrap_or("-"),
                    agent.tools.len(),
                    agent.description
                );
            }
        }
        ProfileAgentCommand::Show { profile, agent_id } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile.data_dir)?;
            let registry = AgentRegistry::load(profile.data_dir.join("agents"))?;
            let agent = registry
                .get(agent_id)
                .ok_or_else(|| anyhow::anyhow!("agent `{agent_id}` not found"))?;
            print_agent(agent);
        }
        ProfileAgentCommand::Upsert { profile, path } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile.data_dir)?;
            let markdown = read_cli_input(path)?;
            let parsed = AgentProfile::from_markdown(&markdown)?;
            validate_file_id(&parsed.id)?;
            let agents_dir = profile.data_dir.join("agents");
            remove_agent_profiles_by_id(&agents_dir, &parsed.id)?;
            let mut registry = AgentRegistry::load(&agents_dir)?;
            let file_name = format!("{}.md", parsed.id);
            let agent = registry.upsert_markdown(&file_name, &markdown)?;
            println!(
                "Saved agent `{}` for profile `{}` to {}",
                agent.id,
                profile.label(),
                agents_dir.join(file_name).display()
            );
        }
        ProfileAgentCommand::SetDefault { profile, agent_id } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile.data_dir)?;
            let registry = AgentRegistry::load(profile.data_dir.join("agents"))?;
            if registry.get(agent_id).is_none() {
                anyhow::bail!("agent `{agent_id}` not found");
            }
            apply_runtime_config_entries(
                &profile,
                &profile.data_dir,
                &[format!("root_agent_id={agent_id}")],
                false,
            )
            .await?;
            println!(
                "Default agent for profile `{}` is `{agent_id}`",
                profile.label()
            );
        }
    }
    Ok(())
}

fn run_profile_workflow_command(
    command: &ProfileWorkflowCommand,
    data_root: &Path,
) -> anyhow::Result<()> {
    match command {
        ProfileWorkflowCommand::List { profile } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile.data_dir)?;
            println!("ID\tNAME\tNODES\tEDGES\tDESCRIPTION");
            println!("goal\tGoal\t2\t1\tEmbedded goal workflow");
            let mut workflows = load_workflow_files(&profile.data_dir)?;
            workflows.sort_by(|a, b| a.id.cmp(&b.id));
            for workflow in workflows {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    workflow.id,
                    workflow.name,
                    workflow.nodes.len(),
                    workflow.edges.len(),
                    workflow.description
                );
            }
        }
        ProfileWorkflowCommand::Show {
            profile,
            workflow_id,
        } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile.data_dir)?;
            let workflow = if workflow_id == "goal" {
                bot_core::supervisor_workflow::embedded_goal_definition()
            } else {
                load_workflow_file(&profile.data_dir, workflow_id)?
            };
            print_workflow(&workflow)?;
        }
        ProfileWorkflowCommand::Upsert { profile, path } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile.data_dir)?;
            let raw = read_cli_input(path)?;
            let workflow: WorkflowDefinition =
                serde_json::from_str(&raw).context("parsing workflow JSON")?;
            workflow
                .validate()
                .map_err(|err| anyhow::anyhow!("invalid workflow: {err}"))?;
            validate_workflow_agents(&profile.data_dir, &workflow)?;
            validate_file_id(&workflow.id)?;
            if workflow.id == "goal" {
                anyhow::bail!("embedded workflow `goal` cannot be overwritten");
            }
            let workflows_dir = profile.data_dir.join("workflows");
            std::fs::create_dir_all(&workflows_dir)
                .with_context(|| format!("creating {}", workflows_dir.display()))?;
            let path = workflows_dir.join(format!("{}.json", workflow.id));
            let json = serde_json::to_string_pretty(&workflow)?;
            std::fs::write(&path, format!("{json}\n"))
                .with_context(|| format!("writing {}", path.display()))?;
            println!(
                "Saved workflow `{}` for profile `{}` to {}",
                workflow.id,
                profile.label(),
                path.display()
            );
        }
        ProfileWorkflowCommand::Delete {
            profile,
            workflow_id,
        } => {
            let profile = profile_from_label(profile, data_root)?;
            let path = profile
                .data_dir
                .join("workflows")
                .join(format!("{workflow_id}.json"));
            if !path.exists() {
                anyhow::bail!("workflow `{workflow_id}` not found");
            }
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            println!(
                "Deleted workflow `{}` from profile `{}`",
                workflow_id,
                profile.label()
            );
        }
    }
    Ok(())
}

fn validate_workflow_agents(data_dir: &Path, workflow: &WorkflowDefinition) -> anyhow::Result<()> {
    let agents_dir = data_dir.join("agents");
    install_embedded_agent_profiles(&agents_dir)?;
    let registry = AgentRegistry::load(&agents_dir)?;
    for node in &workflow.nodes {
        let Some(agent) = node
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if registry.get(agent).is_some() {
            continue;
        }
        anyhow::bail!(
            "workflow node `{}` references unknown agent `{agent}`",
            node.id
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileRunState {
    Running,
    Stopped,
    Stale,
}

impl ProfileRunState {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Stopped => "STOPPED",
            Self::Stale => "STALE",
        }
    }
}

fn profile_run_state(profile: &InstanceProfile) -> ProfileRunState {
    match read_run_metadata(profile) {
        Ok(Some(metadata)) if process_is_alive(metadata.pid) => ProfileRunState::Running,
        Ok(Some(_)) | Err(_) => ProfileRunState::Stale,
        Ok(None) => ProfileRunState::Stopped,
    }
}

fn ensure_profile_assets(data_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    install_embedded_agent_profiles(data_dir.join("agents"))?;
    install_embedded_model_profiles(data_dir.join("models"))?;
    std::fs::create_dir_all(data_dir.join("workflows"))?;
    Ok(())
}

pub fn ensure_builtin_diagnostic_profile() -> anyhow::Result<()> {
    ensure_builtin_diagnostic_profile_in_data_root(Path::new(
        crate::instance_profile::DEFAULT_DATA_DIR,
    ))
}

pub fn ensure_builtin_diagnostic_profile_in_data_root(data_root: &Path) -> anyhow::Result<()> {
    let profile = InstanceProfile::named_in_data_root(DIAGNOSTIC_PROFILE_NAME, data_root)?;
    ensure_profile_assets(&profile.data_dir)?;
    match detect_setup_state(&profile.data_dir) {
        SetupState::Initialized { .. } | SetupState::Invalid { .. } => return Ok(()),
        SetupState::LegacyEnvCompatible { .. } | SetupState::Uninitialized { .. } => {}
    }

    let default_model_profile = match detect_setup_state(data_root) {
        SetupState::Initialized { config, .. } => config.model_profile,
        _ => RuntimeConfig::default_for(data_root).model_profile,
    };

    let mut config = RuntimeConfig::default_for(&profile.data_dir);
    config.root_agent_id = DIAGNOSTIC_PROFILE_NAME.to_string();
    config.model_profile = default_model_profile;
    config.sandbox.kind = RuntimeSandboxKind::NoSandbox;
    config.sandbox.host_dir = ".".to_string();
    config.shell.mode = ShellMode::Local;
    config.im.mode = ImMode::Disabled;
    config.admin.enabled = true;
    config.admin.port = first_available_port(
        &config.admin.host,
        config.admin.port,
        &configured_ports_in_data_root(&profile.data_dir, data_root)?,
    )?;
    write_runtime_config(&profile.data_dir, &config)?;
    Ok(())
}

fn read_cli_input(path: &str) -> anyhow::Result<String> {
    if path == "-" {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .context("reading stdin")?;
        return Ok(input);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {path}"))
}

pub(crate) fn validate_file_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("id `{id}` may only contain ASCII letters, digits, `-`, and `_`");
    }
    Ok(())
}

fn remove_agent_profiles_by_id(agents_dir: &Path, agent_id: &str) -> anyhow::Result<()> {
    if !agents_dir.exists() {
        return Ok(());
    }
    let canonical_target = agents_dir.join(format!("{agent_id}.md"));
    for entry in std::fs::read_dir(agents_dir)
        .with_context(|| format!("reading agent profile dir {}", agents_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        match AgentProfile::from_markdown_file(&path) {
            Ok(profile) if profile.id == agent_id && path != canonical_target => {
                std::fs::remove_file(&path).with_context(|| {
                    format!("removing duplicate agent profile {}", path.display())
                })?;
            }
            Ok(_) | Err(_) => {}
        }
    }
    Ok(())
}

fn load_workflow_files(data_dir: &Path) -> anyhow::Result<Vec<WorkflowDefinition>> {
    let dir = data_dir.join("workflows");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut workflows = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let workflow: WorkflowDefinition = serde_json::from_str(&raw)
            .with_context(|| format!("parsing workflow {}", path.display()))?;
        workflow
            .validate()
            .map_err(|err| anyhow::anyhow!("invalid workflow {}: {err}", path.display()))?;
        workflows.push(workflow);
    }
    Ok(workflows)
}

fn load_workflow_file(data_dir: &Path, workflow_id: &str) -> anyhow::Result<WorkflowDefinition> {
    validate_file_id(workflow_id)?;
    let path = data_dir
        .join("workflows")
        .join(format!("{workflow_id}.json"));
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let workflow: WorkflowDefinition =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    workflow
        .validate()
        .map_err(|err| anyhow::anyhow!("invalid workflow `{workflow_id}`: {err}"))?;
    if workflow.id != workflow_id {
        anyhow::bail!(
            "workflow id `{}` does not match file name `{workflow_id}.json`",
            workflow.id
        );
    }
    Ok(workflow)
}

fn print_agent(agent: &AgentProfile) {
    println!("id: {}", agent.id);
    println!("name: {}", agent.name);
    println!("description: {}", agent.description);
    println!("model: {}", agent.model.as_deref().unwrap_or("-"));
    println!(
        "helper_model: {}",
        agent.models.helper.as_deref().unwrap_or("-")
    );
    println!(
        "vision_model: {}",
        agent.models.vision.as_deref().unwrap_or("-")
    );
    println!("tools: {}", agent.tools.join(", "));
    println!("delegates: {}", agent.delegates.join(", "));
    println!(
        "max_turns: {}",
        agent
            .max_turns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("persistent_sessions: {}", agent.persistent_sessions);
    println!("system_prompt:\n{}", agent.system_prompt);
}

fn print_workflow(workflow: &WorkflowDefinition) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(workflow)?);
    Ok(())
}

fn print_profile_status(profile: &InstanceProfile) -> anyhow::Result<()> {
    let state = detect_setup_state(&profile.data_dir);
    let run_state = profile_run_state(profile);
    println!("profile: {}", profile.label());
    println!("run_status: {}", run_state.label());
    println!("data_dir: {}", profile.data_dir.display());
    println!("pid_file: {}", profile.pid_file().display());
    println!("log_path: {}", profile.log_file().display());
    match read_run_metadata(profile)? {
        Some(metadata) => {
            println!("pid: {}", metadata.pid);
            println!("started_at: {}", metadata.started_at);
            println!("command: {}", metadata.command.join(" "));
        }
        None => println!("pid: -"),
    }
    match state {
        SetupState::Initialized { config, .. } => {
            println!("setup: initialized");
            println!("admin: {}", format_admin_addr(&config));
            println!("im_mode: {}", config.im.mode.as_env_value());
        }
        SetupState::Invalid { error, .. } => {
            println!("setup: invalid");
            println!("error: {error}");
        }
        SetupState::LegacyEnvCompatible { .. } => println!("setup: legacy-env-compatible"),
        SetupState::Uninitialized { .. } => println!("setup: not initialized"),
    }
    Ok(())
}

fn start_profile(profile: &InstanceProfile) -> anyhow::Result<()> {
    match detect_setup_state(&profile.data_dir) {
        SetupState::Initialized { .. } | SetupState::LegacyEnvCompatible { .. } => {}
        SetupState::Invalid { error, .. } => anyhow::bail!("cannot start invalid profile: {error}"),
        SetupState::Uninitialized { .. } => {
            anyhow::bail!("profile `{}` is not initialized", profile.label())
        }
    }
    if let Some(metadata) = read_run_metadata(profile)? {
        if process_is_alive(metadata.pid) {
            anyhow::bail!(
                "profile `{}` is already running with pid {}",
                profile.label(),
                metadata.pid
            );
        }
        remove_run_metadata(profile)?;
    }
    std::fs::create_dir_all(profile.log_dir())
        .with_context(|| format!("creating {}", profile.log_dir().display()))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(profile.log_file())
        .with_context(|| format!("opening {}", profile.log_file().display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("cloning {}", profile.log_file().display()))?;
    let exe = std::env::current_exe().context("resolving current executable")?;
    let mut command = std::process::Command::new(&exe);
    let mut command_display = vec![exe.display().to_string()];
    if let Some(name) = profile.name.as_deref() {
        command.arg("--profile").arg(name);
        command_display.push("--profile".to_string());
        command_display.push(name.to_string());
    }
    for key in PROFILE_RUNTIME_ENV_KEYS {
        command.env_remove(key);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("starting profile `{}`", profile.label()))?;
    let metadata = ProfileRunMetadata {
        pid: child.id(),
        profile: profile.label().to_string(),
        data_dir: profile.data_dir.display().to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        command: command_display,
        log_path: profile.log_file().display().to_string(),
    };
    write_run_metadata(profile, &metadata)?;
    std::thread::sleep(std::time::Duration::from_millis(200));
    if !process_is_alive(metadata.pid) {
        remove_run_metadata(profile)?;
        anyhow::bail!(
            "profile `{}` exited immediately after start; see log at {}",
            profile.label(),
            metadata.log_path
        );
    }
    println!(
        "Started profile `{}` with pid {}. Logs: {}",
        profile.label(),
        metadata.pid,
        metadata.log_path
    );
    Ok(())
}

fn stop_profile(profile: &InstanceProfile, force: bool) -> anyhow::Result<()> {
    let Some(metadata) = read_run_metadata(profile)? else {
        println!("Profile `{}` is not running.", profile.label());
        return Ok(());
    };
    if !process_is_alive(metadata.pid) {
        remove_run_metadata(profile)?;
        println!(
            "Removed stale pid metadata for profile `{}`.",
            profile.label()
        );
        return Ok(());
    }
    let signal = if force { "-KILL" } else { "-TERM" };
    send_signal(metadata.pid, signal)?;
    if wait_for_exit(metadata.pid, std::time::Duration::from_secs(5)) {
        remove_run_metadata(profile)?;
        println!("Stopped profile `{}`.", profile.label());
        return Ok(());
    }
    if force {
        anyhow::bail!(
            "profile `{}` did not exit after SIGKILL; pid {} may require manual inspection",
            profile.label(),
            metadata.pid
        );
    }
    anyhow::bail!(
        "profile `{}` did not stop after SIGTERM; rerun with --force to send SIGKILL",
        profile.label()
    )
}

fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn send_signal(pid: u32, signal: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .with_context(|| format!("sending {signal} to pid {pid}"))?;
    if !status.success() {
        anyhow::bail!("failed to send {signal} to pid {pid}");
    }
    Ok(())
}

fn wait_for_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if !process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    !process_is_alive(pid)
}

pub async fn apply_runtime_config_entries(
    profile: &InstanceProfile,
    data_dir: &Path,
    entries: &[String],
    create_if_missing: bool,
) -> anyhow::Result<()> {
    if entries.is_empty() && !create_if_missing {
        anyhow::bail!("provide at least one key=value entry");
    }
    std::fs::create_dir_all(data_dir)?;
    install_embedded_model_profiles(data_dir.join("models"))?;
    install_embedded_agent_profiles(data_dir.join("agents"))?;
    std::fs::create_dir_all(data_dir.join("workflows"))?;

    let existing_config = match detect_setup_state(data_dir) {
        SetupState::Initialized { config, .. } => Some(config),
        SetupState::Invalid { error, .. } => anyhow::bail!("runtime config is invalid: {error}"),
        _ => None,
    };
    if existing_config.is_none() && !create_if_missing {
        anyhow::bail!(
            "runtime config is not initialized at {}; run `remi-cat{} setup --non-interactive` first",
            data_dir.display(),
            profile
                .name
                .as_ref()
                .map(|name| format!(" --profile {name}"))
                .unwrap_or_default()
        );
    }

    let mut config = existing_config.unwrap_or_else(|| RuntimeConfig::default_for(data_dir));
    config.data_dir = data_dir.display().to_string();
    if profile.is_named() && config.sandbox.container_name == "remi-cat-sandbox" {
        config.sandbox.container_name = format!("remi-cat-sandbox-{}", profile.label());
    }
    let mut model_changed = false;
    for entry in entries {
        if runtime_config_entry_changes_model(entry) {
            model_changed = true;
        }
        apply_runtime_config_entry(&mut config, entry)
            .with_context(|| format!("applying config entry `{entry}`"))?;
    }
    let data_root = profile_data_root(profile);
    normalize_runtime_config(data_dir, &data_root, &mut config)?;
    if model_changed {
        let registry = ModelProfileRegistry::load(data_dir.join("models"))?;
        let model_profile = registry.get(&config.model_profile).ok_or_else(|| {
            anyhow::anyhow!("model profile `{}` does not exist", config.model_profile)
        })?;
        validate_model_profile_api_key(model_profile).await?;
    }
    let path = write_runtime_config(data_dir, &config)?;
    println!(
        "Saved profile `{}` runtime config to {}",
        profile.label(),
        path.display()
    );
    println!("admin: {}", format_admin_addr(&config));
    println!("sandbox_kind: {}", config.sandbox.kind.as_env_value());
    println!("sandbox_container: {}", config.sandbox.container_name);
    Ok(())
}

fn runtime_config_entry_changes_model(entry: &str) -> bool {
    let entry = entry.trim().trim_start_matches("--");
    let Some((raw_key, _)) = entry.split_once('=') else {
        return false;
    };
    matches!(
        raw_key.trim().replace('-', "_").as_str(),
        "model_profile" | "model"
    )
}

fn apply_runtime_config_entry(config: &mut RuntimeConfig, entry: &str) -> anyhow::Result<()> {
    let entry = entry.trim().trim_start_matches("--");
    if entry.is_empty() || entry == "--non-interactive" {
        return Ok(());
    }
    let Some((raw_key, value)) = entry.split_once('=') else {
        anyhow::bail!("expected key=value, got `{entry}`");
    };
    let key = raw_key.trim().replace('-', "_");
    let value = value.trim();
    match key.as_str() {
        "root_agent_id" | "root_agent" | "agent" => config.root_agent_id = value.to_string(),
        "model_profile" | "model" => config.model_profile = value.to_string(),
        "tool_output_overflow_bytes"
        | "tool_output.overflow_bytes"
        | "overflow_bytes"
        | "tool_overflow_bytes" => config.tool_output.overflow_bytes = Some(parse_usize(value)?),
        "tool_foreground_timeout_ms"
        | "tool_output.foreground_timeout_ms"
        | "foreground_timeout_ms"
        | "tool_timeout_ms" => config.tool_output.foreground_timeout_ms = Some(parse_u64(value)?),
        "async_agent" | "tool_output.async_agent" => {
            config.tool_output.async_agent = parse_bool(value)?
        }
        "admin_enabled" | "admin.enabled" => config.admin.enabled = parse_bool(value)?,
        "admin_host" | "admin.host" => config.admin.host = value.to_string(),
        "admin_port" | "admin.port" => config.admin.port = parse_port(value)?,
        "sandbox_kind" | "sandbox.kind" => config.sandbox.kind = parse_sandbox_kind(value)?,
        "sandbox_host_dir" | "sandbox.host_dir" => config.sandbox.host_dir = value.to_string(),
        "sandbox_container_dir" | "sandbox.container_dir" => {
            config.sandbox.container_dir = value.to_string()
        }
        "sandbox_image" | "sandbox.image" => config.sandbox.image = value.to_string(),
        "sandbox_container_name" | "sandbox.container_name" => {
            config.sandbox.container_name = value.to_string()
        }
        "shell_mode" | "shell.mode" => config.shell.mode = parse_shell_mode(value)?,
        "acp_mode" | "acp.mode" => config.acp.mode = parse_acp_mode(value)?,
        "acp_client" | "acp.client" => config.acp.client = parse_acp_client(value)?,
        "acp_tool_name" | "acp.tool_name" | "acp.tool" => {
            config.acp.tool_name = nonempty_optional(value)
        }
        "acp_agent_name" | "acp.agent_name" | "acp.agent" => {
            config.acp.agent_name = nonempty_optional(value)
        }
        "acp_base_url" | "acp.base_url" => config.acp.base_url = nonempty_optional(value),
        "acp_model" | "acp.model" => config.acp.model = nonempty_optional(value),
        "acp_api_key" | "acp.api_key" => config.acp.api_key = nonempty_optional(value),
        "acp_local_bin" | "acp.local_bin" | "local.bin" | "local_bin" => {
            config.acp.local_bin = nonempty_optional(value)
        }
        "acp_local_args" | "acp.local_args" | "local.args" | "local_args" => {
            config.acp.local_args = parse_string_array(value)?
        }
        "acp_codex_bin" | "acp.codex_bin" | "codex.bin" | "codex_bin" => {
            config.acp.codex_bin = nonempty_optional(value)
        }
        "acp_codex_args" | "acp.codex_args" | "codex.args" | "codex_args" => {
            config.acp.codex_args = parse_string_array(value)?
        }
        "im_mode" | "im.mode" => config.im.mode = parse_im_mode(value)?,
        "feishu_transport" | "im_transport" | "im.transport" | "feishu.transport" => {
            config.im.transport = parse_feishu_transport(value)?
        }
        "feishu_hook_host" | "feishu.hook.host" | "im.event_hook.host" => {
            config.im.event_hook.host = value.to_string()
        }
        "feishu_hook_port" | "feishu.hook.port" | "im.event_hook.port" => {
            config.im.event_hook.port = parse_port(value)?
        }
        "feishu_hook_path" | "feishu.hook.path" | "im.event_hook.path" => {
            config.im.event_hook.path = value.to_string()
        }
        "feishu_hook_verification_token" | "feishu.hook.verification_token" => {
            config.im.event_hook.verification_token = value.to_string()
        }
        other => anyhow::bail!("unknown runtime config key `{other}`"),
    }
    Ok(())
}

pub fn prefix_short_config_entry(prefix: &str, entry: &str) -> String {
    let trimmed = entry.trim().trim_start_matches("--");
    let key = trimmed
        .split_once('=')
        .map(|(key, _)| key)
        .unwrap_or(trimmed);
    if key.contains('.') || key.starts_with(&format!("{prefix}_")) {
        trimmed.to_string()
    } else {
        format!("{prefix}.{trimmed}")
    }
}

fn profile_data_root(profile: &InstanceProfile) -> std::path::PathBuf {
    if profile.is_named() {
        profile
            .data_dir
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Path::new(crate::instance_profile::DEFAULT_DATA_DIR).to_path_buf())
    } else {
        profile.data_dir.clone()
    }
}

fn normalize_runtime_config(
    data_dir: &Path,
    data_root: &Path,
    config: &mut RuntimeConfig,
) -> anyhow::Result<()> {
    match config.sandbox.kind {
        RuntimeSandboxKind::Disabled | RuntimeSandboxKind::NoSandbox => {
            if config.sandbox.host_dir.trim().is_empty() {
                config.sandbox.host_dir = data_dir.display().to_string();
            }
        }
        RuntimeSandboxKind::Docker => {
            config.sandbox.container_name = available_container_name_in_data_root(
                &config.sandbox.container_name,
                data_dir,
                data_root,
            )?;
        }
    }
    let mut reserved_ports = configured_ports_in_data_root(data_dir, data_root)?;
    if config.admin.enabled {
        let requested = config.admin.port;
        config.admin.port = first_available_port(&config.admin.host, requested, &reserved_ports)?;
        print_port_adjustment("Admin", requested, config.admin.port);
        reserved_ports.insert(config.admin.port);
    }
    if matches!(config.im.mode, ImMode::Feishu)
        && matches!(config.im.transport, FeishuTransport::EventHook)
    {
        let requested = config.im.event_hook.port;
        config.im.event_hook.port =
            first_available_port(&config.im.event_hook.host, requested, &reserved_ports)?;
        print_port_adjustment("Feishu Event Hook", requested, config.im.event_hook.port);
    }
    Ok(())
}

fn parse_bool(value: &str) -> anyhow::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" => Ok(false),
        _ => anyhow::bail!("expected boolean, got `{value}`"),
    }
}

fn parse_port(value: &str) -> anyhow::Result<u16> {
    value.parse().context("invalid TCP port")
}

fn parse_usize(value: &str) -> anyhow::Result<usize> {
    let parsed = value.parse().context("invalid positive integer")?;
    if parsed == 0 {
        anyhow::bail!("value must be greater than 0");
    }
    Ok(parsed)
}

fn parse_u64(value: &str) -> anyhow::Result<u64> {
    let parsed = value.parse().context("invalid positive integer")?;
    if parsed == 0 {
        anyhow::bail!("value must be greater than 0");
    }
    Ok(parsed)
}

fn parse_sandbox_kind(value: &str) -> anyhow::Result<RuntimeSandboxKind> {
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "disabled" => Ok(RuntimeSandboxKind::Disabled),
        "no_sandbox" | "local" => Ok(RuntimeSandboxKind::NoSandbox),
        "docker" => Ok(RuntimeSandboxKind::Docker),
        _ => anyhow::bail!("unknown sandbox kind `{value}`"),
    }
}

fn parse_shell_mode(value: &str) -> anyhow::Result<ShellMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "disabled" | "off" | "false" => Ok(ShellMode::Disabled),
        "local" | "host" | "true" => Ok(ShellMode::Local),
        other => anyhow::bail!("unknown shell mode `{other}`"),
    }
}

fn parse_im_mode(value: &str) -> anyhow::Result<ImMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "feishu" => Ok(ImMode::Feishu),
        "disabled" | "off" | "none" => Ok(ImMode::Disabled),
        other => anyhow::bail!("unknown IM mode `{other}`"),
    }
}

fn parse_feishu_transport(value: &str) -> anyhow::Result<FeishuTransport> {
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "websocket" | "ws" => Ok(FeishuTransport::WebSocket),
        "event_hook" | "hook" | "webhook" => Ok(FeishuTransport::EventHook),
        _ => anyhow::bail!("unknown Feishu transport `{value}`"),
    }
}

fn parse_acp_mode(value: &str) -> anyhow::Result<AcpMode> {
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "local" | "local_stub" | "stub" => Ok(AcpMode::LocalStub),
        "remote" => Ok(AcpMode::Remote),
        other => anyhow::bail!("unknown ACP mode `{other}`"),
    }
}

fn parse_acp_client(value: &str) -> anyhow::Result<AcpClient> {
    let value = value.trim();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("ACP client may only contain ASCII letters, digits, `-`, and `_`");
    }
    Ok(AcpClient::new(value))
}

fn parse_string_array(value: &str) -> anyhow::Result<Vec<String>> {
    serde_json::from_str::<Vec<String>>(value)
        .with_context(|| "expected a JSON string array, for example [\"--config\",\"key=value\"]")
}

fn nonempty_optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub fn format_admin_addr(config: &RuntimeConfig) -> String {
    if config.admin.enabled {
        format!("http://{}:{}", config.admin.host, config.admin.port)
    } else {
        "disabled".to_string()
    }
}

pub fn configured_ports(data_dir: &Path) -> anyhow::Result<HashSet<u16>> {
    configured_ports_in_data_root(
        data_dir,
        Path::new(crate::instance_profile::DEFAULT_DATA_DIR),
    )
}

pub fn configured_ports_in_data_root(
    data_dir: &Path,
    data_root: &Path,
) -> anyhow::Result<HashSet<u16>> {
    let mut ports = HashSet::new();
    for config in configured_profiles_excluding_in_data_root(data_dir, data_root)? {
        if config.admin.enabled {
            ports.insert(config.admin.port);
        }
        if matches!(config.im.mode, ImMode::Feishu)
            && matches!(config.im.transport, FeishuTransport::EventHook)
        {
            ports.insert(config.im.event_hook.port);
        }
    }
    Ok(ports)
}

pub fn first_available_port(
    host: &str,
    requested: u16,
    reserved: &HashSet<u16>,
) -> anyhow::Result<u16> {
    for candidate in u32::from(requested)..=u32::from(u16::MAX) {
        let candidate = candidate as u16;
        if reserved.contains(&candidate) {
            continue;
        }
        if std::net::TcpListener::bind((host, candidate)).is_ok() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("no available TCP port at or above {requested} for host `{host}`")
}

pub fn print_port_adjustment(label: &str, requested: u16, selected: u16) {
    if selected != requested {
        println!("{label} port {requested} is unavailable; using {selected}.");
    }
}

pub fn available_container_name(requested: &str, data_dir: &Path) -> anyhow::Result<String> {
    available_container_name_in_data_root(
        requested,
        data_dir,
        Path::new(crate::instance_profile::DEFAULT_DATA_DIR),
    )
}

pub fn available_container_name_in_data_root(
    requested: &str,
    data_dir: &Path,
    data_root: &Path,
) -> anyhow::Result<String> {
    let used: HashSet<String> = configured_profiles_excluding_in_data_root(data_dir, data_root)?
        .into_iter()
        .filter(|config| matches!(config.sandbox.kind, RuntimeSandboxKind::Docker))
        .map(|config| config.sandbox.container_name)
        .collect();
    if !used.contains(requested) {
        return Ok(requested.to_string());
    }
    for suffix in 2..=u32::MAX {
        let candidate = format!("{requested}-{suffix}");
        if !used.contains(&candidate) {
            println!("Sandbox container `{requested}` is already configured; using `{candidate}`.");
            return Ok(candidate);
        }
    }
    unreachable!("u32 container-name suffix space exhausted")
}
