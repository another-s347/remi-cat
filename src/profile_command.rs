use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;

use bot_core::{
    install_embedded_agent_profiles, install_embedded_model_profiles,
    validate_model_profile_api_key, AgentProfile, AgentRegistry, ModelProfileRegistry,
    WorkflowDefinition,
};

use crate::instance_profile::{
    configured_profiles_excluding_in_data_root, discover_profiles_in_data_root,
    remove_named_profile_in_data_root, validate_manifest, ApplicationProfileManifest,
    InstanceProfile, ProfileCapabilities, ProfileConfigRefs, ProfileEndpoint, ProfileResourceRefs,
    ProfileStateRefs, DIAGNOSTIC_PROFILE_NAME, PROFILE_FILE_NAME, PROFILE_SCHEMA_VERSION,
};

use crate::profile_registry::ProfileRegistry;
use crate::runtime_config::{
    detect_setup_state, AcpClient, AcpMode, ChannelInstanceConfig, FeishuChannelEventHookConfig,
    FeishuCredentialRefs, FeishuTransport, ImMode, RuntimeConfig, RuntimeSandboxKind, SetupState,
    ShellMode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileCommand {
    Current,
    List {
        scope: String,
        format: String,
    },
    Find {
        tags: Vec<String>,
        intents: Vec<String>,
        channel: Option<String>,
        format: String,
    },
    Show {
        reference: Option<String>,
        view: ProfileShowView,
        format: String,
    },
    Check {
        reference: Option<String>,
        strict: bool,
        format: String,
    },
    Init {
        directory: String,
        id: Option<String>,
        name: Option<String>,
        template: String,
        register: Option<String>,
        with_runtime: bool,
    },
    Register {
        path: String,
        alias: Option<String>,
        replace: bool,
    },
    Unregister {
        reference: String,
    },
    Set {
        reference: String,
        field: String,
        value: String,
        dry_run: bool,
    },
    Unset {
        reference: String,
        field: String,
        dry_run: bool,
    },
    Ask {
        reference: String,
        task: String,
        named: String,
        agent_id: Option<String>,
    },
    Start {
        reference: String,
        instance: String,
    },
    Stop {
        reference: String,
        instance: String,
        force: bool,
    },
    Restart {
        reference: String,
        instance: String,
        force: bool,
    },
    Status {
        reference: Option<String>,
        all: bool,
        instance: Option<String>,
        format: String,
    },
    Channel(ProfileChannelCommand),
    Create {
        name: String,
        entries: Vec<String>,
    },
    Delete {
        name: String,
        force: bool,
    },
    Resource(ProfileResourceCommand),
    Registry(ProfileRegistryCommand),
    Agent(ProfileAgentCommand),
    Workflow(ProfileWorkflowCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileChannelCommand {
    List {
        reference: String,
        format: String,
    },
    UpsertFeishu {
        reference: String,
        id: String,
        enabled: bool,
        transport: FeishuTransport,
        app_id_env: String,
        app_secret_env: String,
        host: String,
        port: u16,
        path: String,
        verification_token_env: Option<String>,
    },
    Enable {
        reference: String,
        id: String,
    },
    Disable {
        reference: String,
        id: String,
    },
    Remove {
        reference: String,
        id: String,
        force: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileShowView {
    Summary,
    Manifest,
    Resolved,
    Sources,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileResourceCommand {
    List { reference: String },
    Show { reference: String, resource: String },
    Check { reference: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileRegistryCommand {
    Info,
    List,
    Repair,
    RebuildIndex,
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

pub async fn run_profile_command(
    command: &ProfileCommand,
    data_root: &Path,
    current: &InstanceProfile,
    profile_hubs: &[crate::profile_hub::ProfileHubClient],
) -> anyhow::Result<()> {
    let mut registry = ProfileRegistry::load(data_root)?;
    match command {
        ProfileCommand::Current => print_current_profile(current, &registry)?,
        ProfileCommand::List { scope, format } => {
            print_profile_list(data_root, &registry, scope, format)?
        }
        ProfileCommand::Find {
            tags,
            intents,
            channel,
            format,
        } => print_profile_find(
            data_root,
            &registry,
            tags,
            intents,
            channel.as_deref(),
            format,
        )?,
        ProfileCommand::Show {
            reference,
            view,
            format,
        } => {
            let profile = resolve_or_current(reference.as_deref(), current, &registry)?;
            print_profile_show(&profile, &registry, *view, format)?;
        }
        ProfileCommand::Check {
            reference,
            strict,
            format,
        } => {
            let profile = resolve_or_current(reference.as_deref(), current, &registry)?;
            check_profile(&profile, *strict, format)?;
        }
        ProfileCommand::Init {
            directory,
            id,
            name,
            template,
            register,
            with_runtime,
        } => {
            init_profile(
                data_root,
                directory,
                id.as_deref(),
                name.as_deref(),
                template,
                register.as_deref(),
                *with_runtime,
            )?;
        }
        ProfileCommand::Register {
            path,
            alias,
            replace,
        } => {
            let entry = registry.register(Path::new(path), alias.as_deref(), *replace)?;
            println!(
                "Registered @{}\n  ID: {}\n  Manifest: {}\n\nNo files were copied.",
                entry.alias,
                entry.id,
                entry.manifest_path.display()
            );
        }
        ProfileCommand::Unregister { reference } => {
            let entry = registry.unregister(reference)?;
            println!(
                "Unregistered @{}.\n\nPreserved:\n  {}\n  all referenced resources and state",
                entry.alias,
                entry.manifest_path.display()
            );
        }
        ProfileCommand::Set {
            reference,
            field,
            value,
            dry_run,
        } => {
            edit_manifest(&mut registry, reference, field, Some(value), *dry_run)?;
        }
        ProfileCommand::Unset {
            reference,
            field,
            dry_run,
        } => {
            edit_manifest(&mut registry, reference, field, None, *dry_run)?;
        }
        ProfileCommand::Ask {
            reference,
            task,
            named,
            agent_id,
        } => {
            let answer = crate::external_agent::ask_profile(
                data_root,
                &current.manifest.id,
                reference,
                task,
                named,
                agent_id.as_deref(),
                profile_hubs,
            )
            .await?;
            println!("{answer}");
        }
        ProfileCommand::Start {
            reference,
            instance,
        } => {
            let status = crate::profile_instance::start(data_root, &registry, reference, instance)?;
            print_instance_status(&status);
        }
        ProfileCommand::Stop {
            reference,
            instance,
            force,
        } => {
            let status =
                crate::profile_instance::stop(data_root, &registry, reference, instance, *force)?;
            print_instance_status(&status);
        }
        ProfileCommand::Restart {
            reference,
            instance,
            force,
        } => {
            let status = crate::profile_instance::restart(
                data_root, &registry, reference, instance, *force,
            )?;
            print_instance_status(&status);
        }
        ProfileCommand::Status {
            reference,
            all,
            instance,
            format,
        } => {
            let statuses = crate::profile_instance::status(
                data_root,
                &registry,
                reference.as_deref(),
                instance.as_deref(),
                *all,
            )?;
            print_instance_statuses(&statuses, format)?;
        }
        ProfileCommand::Channel(command) => run_profile_channel_command(command, &registry)?,
        ProfileCommand::Create { name, entries } => {
            eprintln!("Warning: `profile create` is deprecated; use `profile init`, `profile register`, and `setup --profile`.");
            let profile = InstanceProfile::named_in_data_root(name, data_root)?;
            if matches!(
                detect_setup_state(&profile.data_dir),
                SetupState::Initialized { .. }
            ) {
                anyhow::bail!("profile `{name}` already exists");
            }
            apply_runtime_config_entries(&profile, &profile.data_dir, entries, true).await?;
            let path = profile.write_manifest()?;
            println!("Saved application profile manifest to {}", path.display());
        }
        ProfileCommand::Delete { name, force } => {
            eprintln!("Warning: `profile delete` is deprecated because profiles do not own referenced resources; use `profile unregister`.");
            if !force {
                anyhow::bail!("refusing to delete profile `{name}` without --force");
            }
            let path = remove_named_profile_in_data_root(name, data_root)?;
            println!("Deleted profile `{name}` at {}", path.display());
        }
        ProfileCommand::Resource(command) => run_profile_resource_command(command, &registry)?,
        ProfileCommand::Registry(command) => run_profile_registry_command(command, &mut registry)?,
        ProfileCommand::Agent(command) => {
            eprintln!(
                "Warning: `profile agent` is deprecated; use the agent command with --profile."
            );
            run_profile_agent_command(command, data_root).await?
        }
        ProfileCommand::Workflow(command) => {
            eprintln!("Warning: `profile workflow` is deprecated; use `workflow --profile`.");
            run_profile_workflow_command(command, data_root)?
        }
    }
    Ok(())
}

fn print_instance_status(status: &crate::profile_instance::ProfileInstanceStatus) {
    println!("Profile:  {}", status.profile_id);
    println!("Instance: {}", status.instance);
    println!("State:    {}", status.state);
    println!("PID:      {}", status.pid);
    println!("Started:  {}", status.started_at);
    println!("Log:      {}", status.log_path.display());
}

fn print_instance_statuses(
    statuses: &[crate::profile_instance::ProfileInstanceStatus],
    format: &str,
) -> anyhow::Result<()> {
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(statuses)?);
        return Ok(());
    }
    if statuses.is_empty() {
        println!("No managed profile instances.");
        return Ok(());
    }
    println!("PROFILE\tINSTANCE\tSTATE\tPID\tSTARTED\tLOG");
    for status in statuses {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            status.profile_id,
            status.instance,
            status.state,
            status.pid,
            status.started_at,
            status.log_path.display()
        );
    }
    Ok(())
}

fn run_profile_channel_command(
    command: &ProfileChannelCommand,
    registry: &ProfileRegistry,
) -> anyhow::Result<()> {
    let reference = match command {
        ProfileChannelCommand::List { reference, .. }
        | ProfileChannelCommand::UpsertFeishu { reference, .. }
        | ProfileChannelCommand::Enable { reference, .. }
        | ProfileChannelCommand::Disable { reference, .. }
        | ProfileChannelCommand::Remove { reference, .. } => reference,
    };
    let profile = registry.resolve(reference)?;
    match command {
        ProfileChannelCommand::List { format, .. } => {
            let config = crate::runtime_config::load_channels_config_at(&profile.channels_config)?
                .unwrap_or_default();
            match format.as_str() {
                "json" => println!("{}", serde_json::to_string_pretty(&config)?),
                "yaml" => print!("{}", serde_yaml::to_string(&config)?),
                _ if config.channels.is_empty() => println!("No configured channel instances."),
                _ => {
                    println!("ID\tKIND\tENABLED\tTRANSPORT\tCREDENTIALS");
                    for channel in config.channels {
                        match channel {
                            ChannelInstanceConfig::Feishu {
                                id,
                                enabled,
                                transport,
                                credentials,
                                ..
                            } => println!(
                                "{id}\tfeishu\t{enabled}\t{}\t{}/{}",
                                transport.as_env_value(),
                                credentials.app_id_env,
                                credentials.app_secret_env
                            ),
                        }
                    }
                }
            }
        }
        ProfileChannelCommand::UpsertFeishu {
            id,
            enabled,
            transport,
            app_id_env,
            app_secret_env,
            host,
            port,
            path,
            verification_token_env,
            ..
        } => {
            let mut config =
                crate::runtime_config::load_channels_config_at(&profile.channels_config)?
                    .unwrap_or_default();
            let replacement = ChannelInstanceConfig::Feishu {
                id: id.clone(),
                enabled: *enabled,
                transport: transport.clone(),
                event_hook: FeishuChannelEventHookConfig {
                    host: host.clone(),
                    port: *port,
                    path: path.clone(),
                    verification_token_env: verification_token_env.clone(),
                },
                credentials: FeishuCredentialRefs {
                    app_id_env: app_id_env.clone(),
                    app_secret_env: app_secret_env.clone(),
                },
            };
            if let Some(existing) = config
                .channels
                .iter_mut()
                .find(|channel| channel.id() == id)
            {
                *existing = replacement;
            } else {
                config.channels.push(replacement);
            }
            crate::runtime_config::write_channels_config_at(&profile.channels_config, &config)?;
            println!(
                "Saved Feishu channel `{id}` for {} at {} (enabled={enabled}, transport={}).",
                profile.manifest.id,
                profile.channels_config.display(),
                transport.as_env_value()
            );
            if !profile
                .manifest
                .capabilities
                .channels
                .iter()
                .any(|channel| channel == "feishu")
            {
                println!(
                    "Next: declare discovery metadata with `profile set {reference} capabilities.channels feishu`."
                );
            }
        }
        ProfileChannelCommand::Enable { id, .. } => {
            set_channel_enabled(&profile, id, true)?;
        }
        ProfileChannelCommand::Disable { id, .. } => {
            set_channel_enabled(&profile, id, false)?;
        }
        ProfileChannelCommand::Remove { id, force, .. } => {
            if !force {
                anyhow::bail!("refusing to remove channel `{id}` without --force")
            }
            let mut config =
                crate::runtime_config::load_channels_config_at(&profile.channels_config)?
                    .unwrap_or_default();
            let before = config.channels.len();
            config.channels.retain(|channel| channel.id() != id);
            if config.channels.len() == before {
                anyhow::bail!(
                    "channel `{id}` is not configured for {}",
                    profile.manifest.id
                )
            }
            crate::runtime_config::write_channels_config_at(&profile.channels_config, &config)?;
            println!("Removed channel `{id}` from {}.", profile.manifest.id);
        }
    }
    Ok(())
}

fn set_channel_enabled(profile: &InstanceProfile, id: &str, enabled: bool) -> anyhow::Result<()> {
    let mut config = crate::runtime_config::load_channels_config_at(&profile.channels_config)?
        .unwrap_or_default();
    let channel = config
        .channels
        .iter_mut()
        .find(|channel| channel.id() == id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "channel `{id}` is not configured for {}",
                profile.manifest.id
            )
        })?;
    match channel {
        ChannelInstanceConfig::Feishu {
            enabled: current, ..
        } => *current = enabled,
    }
    crate::runtime_config::write_channels_config_at(&profile.channels_config, &config)?;
    println!(
        "{} channel `{id}` for {}. Restart its managed instance to apply the change.",
        if enabled { "Enabled" } else { "Disabled" },
        profile.manifest.id
    );
    Ok(())
}

fn resolve_or_current(
    reference: Option<&str>,
    current: &InstanceProfile,
    registry: &ProfileRegistry,
) -> anyhow::Result<InstanceProfile> {
    match reference {
        Some(reference) => registry.resolve(reference),
        None => Ok(current.clone()),
    }
}

fn print_current_profile(
    current: &InstanceProfile,
    registry: &ProfileRegistry,
) -> anyhow::Result<()> {
    let registered = current
        .manifest_path
        .as_deref()
        .and_then(|path| registry.registration_for_path(path));
    println!(
        "Profile:        {}",
        registered
            .map(|entry| format!("@{}", entry.alias))
            .unwrap_or_else(|| current.label().to_string())
    );
    println!("ID:             {}", current.manifest.id);
    println!(
        "Manifest:       {}",
        current
            .manifest_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "builtin".to_string())
    );
    println!(
        "Selected by:    {}",
        if current.manifest_path.is_some() {
            "--profile or registered/path selection"
        } else {
            "builtin/default data-root selection"
        }
    );
    println!(
        "Registry alias: {}",
        registered
            .map(|entry| format!("@{}", entry.alias))
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "Workspace:      {}",
        current
            .workspace
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "Check:          {}",
        if profile_check_issues(current).0.is_empty() {
            "OK"
        } else {
            "FAILED"
        }
    );
    println!("\nSelection precedence:\n  1. --profile\n  2. REMI_PROFILE\n  3. REMI_DATA_DIR\n  4. builtin default");
    Ok(())
}

fn collect_profiles(
    data_root: &Path,
    registry: &ProfileRegistry,
    scope: &str,
) -> anyhow::Result<Vec<(String, InstanceProfile)>> {
    let mut profiles = Vec::new();
    if scope != "registered" {
        for profile in discover_profiles_in_data_root(data_root)? {
            profiles.push((profile.label().to_string(), profile));
        }
    }
    if scope != "legacy" {
        for entry in registry.entries() {
            match InstanceProfile::from_manifest(&entry.manifest_path) {
                Ok(profile) => profiles.push((format!("@{}", entry.alias), profile)),
                Err(err) => eprintln!("Warning: @{}: {err:#}", entry.alias),
            }
        }
    }
    profiles.sort_by(|a, b| a.0.cmp(&b.0));
    profiles.dedup_by(|a, b| a.0 == b.0);
    Ok(profiles)
}

fn profile_summary_json(reference: &str, profile: &InstanceProfile) -> serde_json::Value {
    let (errors, warnings) = profile_check_issues(profile);
    serde_json::json!({
        "reference": reference,
        "id": profile.manifest.id,
        "name": profile.manifest.name,
        "manifest": profile.manifest_path,
        "check": if errors.is_empty() { if warnings.is_empty() { "OK" } else { "WARN" } } else { "FAIL" },
        "endpoint_type": match profile.endpoint { ProfileEndpoint::Local { .. } => "local", ProfileEndpoint::Remote { .. } => "remote" },
        "channels": profile.manifest.capabilities.channels,
    })
}

fn print_profile_list(
    data_root: &Path,
    registry: &ProfileRegistry,
    scope: &str,
    format: &str,
) -> anyhow::Result<()> {
    let profiles = collect_profiles(data_root, registry, scope)?;
    let values = profiles
        .iter()
        .map(|(reference, profile)| profile_summary_json(reference, profile))
        .collect::<Vec<_>>();
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else if format == "yaml" {
        println!("{}", serde_yaml::to_string(&values)?);
    } else {
        if format == "table" {
            println!("REF\tID\tNAME\tCHECK\tENDPOINT\tCHANNELS\tMANIFEST");
        }
        for (reference, profile) in profiles {
            let (errors, warnings) = profile_check_issues(&profile);
            let check = if !errors.is_empty() {
                "FAIL"
            } else if !warnings.is_empty() {
                "WARN"
            } else {
                "OK"
            };
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                reference,
                profile.manifest.id,
                profile.manifest.name,
                check,
                match profile.endpoint {
                    ProfileEndpoint::Local { .. } => "local",
                    ProfileEndpoint::Remote { .. } => "remote",
                },
                profile.manifest.capabilities.channels.join(","),
                profile
                    .manifest_path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "builtin".to_string())
            );
        }
    }
    Ok(())
}

fn print_profile_find(
    data_root: &Path,
    registry: &ProfileRegistry,
    tags: &[String],
    intents: &[String],
    channel: Option<&str>,
    format: &str,
) -> anyhow::Result<()> {
    let matches = collect_profiles(data_root, registry, "all")?
        .into_iter()
        .filter(|(_, profile)| {
            tags.iter()
                .all(|tag| profile.manifest.capabilities.tags.contains(tag))
        })
        .filter(|(_, profile)| {
            intents
                .iter()
                .all(|intent| profile.manifest.capabilities.intents.contains(intent))
        })
        .filter(|(_, profile)| {
            channel
                .map(|channel| {
                    profile
                        .manifest
                        .capabilities
                        .channels
                        .iter()
                        .any(|value| value == channel)
                })
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    let values = matches
        .iter()
        .map(|(reference, profile)| profile_summary_json(reference, profile))
        .collect::<Vec<_>>();
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else if format == "yaml" {
        println!("{}", serde_yaml::to_string(&values)?);
    } else {
        if format == "table" {
            println!("REF\tID\tNAME\tCHANNELS");
        }
        for (reference, profile) in matches {
            println!(
                "{}\t{}\t{}\t{}",
                reference,
                profile.manifest.id,
                profile.manifest.name,
                profile.manifest.capabilities.channels.join(",")
            );
        }
    }
    Ok(())
}

fn resolved_profile_json(profile: &InstanceProfile) -> serde_json::Value {
    let channel_instances = effective_channels_config(profile)
        .map(|config| config.channels)
        .unwrap_or_default();
    serde_json::json!({
        "id": profile.manifest.id,
        "name": profile.manifest.name,
        "manifest": profile.manifest_path,
        "workspace": profile.workspace,
        "config": { "runtime": profile.runtime_config, "channels": profile.channels_config },
        "channel_instances": channel_instances,
        "resources": { "agents": profile.agents_dir, "models": profile.models_dir, "skills": profile.skills_dirs, "workflows": profile.workflows_dir },
        "state": { "data": profile.data_dir, "sessions": profile.sessions_path, "memory": profile.memory_dir, "users": profile.users_path, "tasks": profile.tasks_dir },
        "capabilities": profile.manifest.capabilities,
        "endpoint": profile.endpoint,
    })
}

fn print_profile_show(
    profile: &InstanceProfile,
    registry: &ProfileRegistry,
    view: ProfileShowView,
    format: &str,
) -> anyhow::Result<()> {
    if matches!(view, ProfileShowView::Manifest) {
        if format == "json" {
            println!("{}", serde_json::to_string_pretty(&profile.manifest)?);
        } else {
            println!("{}", serde_yaml::to_string(&profile.manifest)?);
        }
        return Ok(());
    }
    let resolved = resolved_profile_json(profile);
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&resolved)?);
        return Ok(());
    }
    if format == "yaml" {
        println!("{}", serde_yaml::to_string(&resolved)?);
        return Ok(());
    }
    if matches!(view, ProfileShowView::Sources) {
        println!("FIELD\tDECLARED\tRESOLVED\tSOURCE");
        print_source_row(
            "workspace",
            profile.manifest.workspace.as_deref(),
            profile.workspace.as_deref(),
            "manifest",
        );
        print_source_row(
            "config.runtime",
            profile.manifest.config.runtime.as_deref(),
            Some(&profile.runtime_config),
            if profile.manifest.config.runtime.is_some() {
                "manifest"
            } else {
                "state.data default"
            },
        );
        print_source_row(
            "config.channels",
            profile.manifest.config.channels.as_deref(),
            Some(&profile.channels_config),
            if profile.manifest.config.channels.is_some() {
                "manifest"
            } else {
                "state.data default"
            },
        );
        print_source_row(
            "resources.agents",
            profile.manifest.resources.agents.as_deref(),
            Some(&profile.agents_dir),
            if profile.manifest.resources.agents.is_some() {
                "manifest"
            } else {
                "state.data default"
            },
        );
        print_source_row(
            "resources.models",
            profile.manifest.resources.models.as_deref(),
            Some(&profile.models_dir),
            if profile.manifest.resources.models.is_some() {
                "manifest"
            } else {
                "state.data default"
            },
        );
        print_source_row(
            "state.sessions",
            profile.manifest.state.sessions.as_deref(),
            Some(&profile.sessions_path),
            if profile.manifest.state.sessions.is_some() {
                "manifest"
            } else {
                "state.data default"
            },
        );
        print_source_row(
            "state.memory",
            profile.manifest.state.memory.as_deref(),
            Some(&profile.memory_dir),
            if profile.manifest.state.memory.is_some() {
                "manifest"
            } else {
                "state.data default"
            },
        );
        return Ok(());
    }
    let registration = profile
        .manifest_path
        .as_deref()
        .and_then(|path| registry.registration_for_path(path));
    let (errors, warnings) = profile_check_issues(profile);
    println!("{}\n  ID:          {}\n  Manifest:    {}\n  Registered:  {}\n  Workspace:   {}\n  Check:       {}", profile.manifest.name, profile.manifest.id, profile.manifest_path.as_deref().map(|path| path.display().to_string()).unwrap_or_else(|| "builtin".to_string()), registration.map(|entry| format!("@{}", entry.alias)).unwrap_or_else(|| "no".to_string()), profile.workspace.as_deref().map(|path| path.display().to_string()).unwrap_or_else(|| "-".to_string()), if !errors.is_empty() { "FAIL" } else if !warnings.is_empty() { "WARN" } else { "OK" });
    println!(
        "\nCapabilities\n  Tags:        {}\n  Intents:     {}\n  Channels:    {}",
        profile.manifest.capabilities.tags.join(", "),
        profile.manifest.capabilities.intents.join(", "),
        profile.manifest.capabilities.channels.join(", ")
    );
    println!(
        "\nConfiguration\n  Runtime:     {}\n  Channels:    {}",
        profile.runtime_config.display(),
        profile.channels_config.display()
    );
    if let Some(config) = effective_channels_config(profile) {
        println!("\nChannel instances");
        if config.channels.is_empty() {
            println!("  (none)");
        }
        for channel in config.channels {
            match channel {
                crate::runtime_config::ChannelInstanceConfig::Feishu {
                    id,
                    enabled,
                    transport,
                    credentials,
                    ..
                } => println!(
                    "  {id}: feishu, enabled={enabled}, transport={}, credentials={}/{}",
                    transport.as_env_value(),
                    credentials.app_id_env,
                    credentials.app_secret_env
                ),
            }
        }
    }
    println!(
        "\nResources\n  Agents:      {}\n  Models:      {}\n  Skills:      {}\n  Workflows:   {}",
        profile.agents_dir.display(),
        profile.models_dir.display(),
        profile
            .skills_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        profile.workflows_dir.display()
    );
    println!(
        "\nState\n  Sessions:    {}\n  Memory:      {}\n  Users:       {}\n  Tasks:       {}",
        profile.sessions_path.display(),
        profile.memory_dir.display(),
        profile.users_path.display(),
        profile.tasks_dir.display()
    );
    match &profile.endpoint {
        ProfileEndpoint::Local { .. } => {
            println!("\nEndpoint\n  local: {}", profile.expanded_local_command()?)
        }
        ProfileEndpoint::Remote { url, .. } => {
            println!("\nEndpoint\n  remote: {url} (not implemented)")
        }
    }
    Ok(())
}

fn print_source_row(field: &str, declared: Option<&Path>, resolved: Option<&Path>, source: &str) {
    println!(
        "{}\t{}\t{}\t{}",
        field,
        declared
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "-".to_string()),
        resolved
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "-".to_string()),
        source
    );
}

fn profile_check_issues(profile: &InstanceProfile) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    if let ProfileEndpoint::Local { command } = &profile.endpoint {
        if command.trim().is_empty() {
            errors.push("endpoint.command is empty".to_string());
        }
    }
    if let Some(workspace) = profile.workspace.as_deref() {
        if !workspace.is_dir() {
            errors.push(format!("workspace does not exist: {}", workspace.display()));
        }
    }
    for (label, path) in [
        ("agents", &profile.agents_dir),
        ("models", &profile.models_dir),
        ("workflows", &profile.workflows_dir),
    ] {
        if !path.exists() {
            warnings.push(format!("{label} path does not exist: {}", path.display()));
        } else if !path.is_dir() {
            errors.push(format!(
                "{label} path is not a directory: {}",
                path.display()
            ));
        }
    }
    for path in &profile.skills_dirs {
        if !path.exists() {
            warnings.push(format!("skill path does not exist: {}", path.display()));
        }
    }
    if profile.runtime_config.exists() {
        match crate::runtime_config::load_runtime_config_at(
            &profile.runtime_config,
            &profile.data_dir,
        ) {
            Ok(Some(config)) => {
                if let Ok(agents) = AgentRegistry::load(&profile.agents_dir) {
                    if agents.get(&config.root_agent_id).is_none() {
                        errors.push(format!("root agent `{}` not found", config.root_agent_id));
                    }
                }
                if let Ok(models) = ModelProfileRegistry::load(&profile.models_dir) {
                    if models.get(&config.model_profile).is_none() {
                        errors.push(format!(
                            "model profile `{}` not found",
                            config.model_profile
                        ));
                    }
                }
            }
            Ok(None) => warnings.push(format!(
                "runtime config is empty: {}",
                profile.runtime_config.display()
            )),
            Err(err) => errors.push(format!("invalid runtime config: {err:#}")),
        }
    } else {
        warnings.push(format!(
            "runtime config does not exist: {}",
            profile.runtime_config.display()
        ));
    }
    if profile.channels_config.exists() {
        match crate::runtime_config::load_channels_config_at(&profile.channels_config) {
            Ok(Some(config)) => {
                if config.channels.iter().any(|channel| {
                    matches!(
                        channel,
                        crate::runtime_config::ChannelInstanceConfig::Feishu { enabled: true, .. }
                    )
                }) && !profile
                    .manifest
                    .capabilities
                    .channels
                    .iter()
                    .any(|channel| channel == "feishu")
                {
                    warnings.push(
                        "enabled Feishu instances are not declared in capabilities.channels"
                            .to_string(),
                    );
                }
            }
            Ok(None) => {}
            Err(err) => errors.push(format!("invalid channel config: {err:#}")),
        }
    } else if profile.manifest.config.channels.is_some() {
        warnings.push(format!(
            "channel config does not exist: {}",
            profile.channels_config.display()
        ));
    }
    (errors, warnings)
}

fn effective_channels_config(
    profile: &InstanceProfile,
) -> Option<crate::runtime_config::ChannelsConfig> {
    if let Ok(Some(config)) =
        crate::runtime_config::load_channels_config_at(&profile.channels_config)
    {
        return Some(config);
    }
    crate::runtime_config::load_runtime_config_at(&profile.runtime_config, &profile.data_dir)
        .ok()
        .flatten()
        .map(|runtime| crate::runtime_config::ChannelsConfig::from_legacy(&runtime.im))
}

fn check_profile(profile: &InstanceProfile, strict: bool, format: &str) -> anyhow::Result<()> {
    let (errors, warnings) = profile_check_issues(profile);
    if format == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"ok": errors.is_empty() && (!strict || warnings.is_empty()), "profile_id": profile.manifest.id, "errors": errors, "warnings": warnings})
            )?
        );
    } else {
        println!("Profile check: {}\n", profile.manifest.id);
        println!(
            "PASS  manifest.schema       schema_version {}",
            profile.manifest.schema_version
        );
        println!("PASS  manifest.identity     {}", profile.manifest.id);
        for warning in &warnings {
            println!("WARN  {warning}");
        }
        for error in &errors {
            println!("FAIL  {error}");
        }
        println!(
            "\nResult: {}",
            if errors.is_empty() && (!strict || warnings.is_empty()) {
                "OK"
            } else {
                "FAILED"
            }
        );
    }
    if !errors.is_empty() || (strict && !warnings.is_empty()) {
        anyhow::bail!(
            "PROFILE_CHECK_FAILED: profile has {} error(s) and {} warning(s)",
            errors.len(),
            warnings.len()
        );
    }
    Ok(())
}

fn init_profile(
    data_root: &Path,
    directory: &str,
    id: Option<&str>,
    name: Option<&str>,
    template: &str,
    register: Option<&str>,
    with_runtime: bool,
) -> anyhow::Result<()> {
    let directory = PathBuf::from(directory);
    let path = if directory
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| matches!(v, "yaml" | "yml"))
        .unwrap_or(false)
    {
        directory
    } else {
        directory.join(PROFILE_FILE_NAME)
    };
    if path.exists() {
        anyhow::bail!("PROFILE_EXISTS: {} already exists", path.display());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let _lock = ManifestLock::acquire(&path)?;
    if path.exists() {
        anyhow::bail!("PROFILE_EXISTS: {} already exists", path.display());
    }
    let inferred = parent
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|v| !v.is_empty())
        .unwrap_or("remi");
    let id = id.unwrap_or(inferred).to_string();
    let name = name.unwrap_or(inferred).to_string();
    let remi_template = template == "remi-cat";
    let manifest = ApplicationProfileManifest {
        schema_version: PROFILE_SCHEMA_VERSION,
        id,
        name,
        description: None,
        version: None,
        workspace: remi_template.then(|| PathBuf::from(".")),
        config: ProfileConfigRefs {
            runtime: remi_template.then(|| PathBuf::from("runtime.yaml")),
            channels: remi_template.then(|| PathBuf::from("channels.yaml")),
        },
        resources: if remi_template {
            ProfileResourceRefs {
                agents: Some("agents".into()),
                models: Some("models".into()),
                skills: vec!["skills".into()],
                workflows: Some("workflows".into()),
            }
        } else {
            ProfileResourceRefs::default()
        },
        state: if remi_template {
            ProfileStateRefs {
                data: Some("state".into()),
                sessions: Some("state/sessions.json".into()),
                memory: Some("state/memory".into()),
                users: Some("state/users.json".into()),
                tasks: Some("state/tool_tasks".into()),
            }
        } else {
            ProfileStateRefs::default()
        },
        capabilities: ProfileCapabilities {
            tags: if template == "external" {
                vec!["external".to_string()]
            } else {
                vec!["general".to_string()]
            },
            intents: Vec::new(),
            channels: if remi_template {
                vec!["tui".into(), "web".into(), "feishu".into(), "acp".into()]
            } else {
                Vec::new()
            },
        },
        endpoint: ProfileEndpoint::Local {
            command: if remi_template {
                "remi-cat --profile \"${PROFILE_DIR}/profile.yaml\" a2a stdio".to_string()
            } else {
                "replace-with-agent-command --a2a-stdio".to_string()
            },
        },
    };
    validate_manifest(&manifest)?;
    write_manifest_atomic(&path, &serde_yaml::to_string(&manifest)?)?;
    let profile = InstanceProfile::from_manifest(&path)?;
    if with_runtime {
        install_embedded_agent_profiles(&profile.agents_dir)?;
        install_embedded_model_profiles(&profile.models_dir)?;
        for skills_dir in &profile.skills_dirs {
            std::fs::create_dir_all(skills_dir)
                .with_context(|| format!("creating {}", skills_dir.display()))?;
        }
        std::fs::create_dir_all(&profile.workflows_dir)?;
        crate::runtime_config::write_runtime_config_at(
            &profile.runtime_config,
            &RuntimeConfig::default_for(&profile.data_dir),
        )?;
        crate::runtime_config::write_channels_config_at(
            &profile.channels_config,
            &crate::runtime_config::ChannelsConfig::from_legacy(
                &RuntimeConfig::default_for(&profile.data_dir).im,
            ),
        )?;
    }
    println!("Created: {}", path.display());
    if let Some(alias) = register {
        let mut registry = ProfileRegistry::load(data_root)?;
        let entry = registry.register(&path, Some(alias), false)?;
        println!("Registered: @{}", entry.alias);
    } else {
        println!("\nNot performed:\n  Profile was not registered.\n\nNext:\n  remi-cat profile check {}\n  remi-cat profile register {}", path.display(), path.display());
    }
    Ok(())
}

fn edit_manifest(
    registry: &mut ProfileRegistry,
    reference: &str,
    field: &str,
    value: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let profile = registry.resolve(reference)?;
    let path = profile.manifest_path.clone().ok_or_else(|| {
        anyhow::anyhow!("builtin profiles cannot be edited; initialize a profile.yaml first")
    })?;
    let registered_alias = registry
        .registration_for_path(&path)
        .map(|entry| entry.alias.clone());
    let _lock = ManifestLock::acquire(&path)?;
    let profile = InstanceProfile::from_manifest(&path)?;
    let mut manifest = profile.manifest;
    set_manifest_field(&mut manifest, field, value)?;
    validate_manifest(&manifest)?;
    let raw = serde_yaml::to_string(&manifest)?;
    if dry_run {
        println!("Would update {}:\n{}", path.display(), raw);
        return Ok(());
    }
    write_manifest_atomic(&path, &raw)?;
    InstanceProfile::from_manifest(&path)?;
    if let Some(alias) = registered_alias.as_deref() {
        registry.register(&path, Some(alias), true)?;
    }
    println!(
        "Updated {}\n  Field: {}\n  Value: {}",
        path.display(),
        field,
        value.unwrap_or("<unset>")
    );
    Ok(())
}

struct ManifestLock {
    path: PathBuf,
}

impl ManifestLock {
    fn acquire(manifest_path: &Path) -> anyhow::Result<Self> {
        let file_name = manifest_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(PROFILE_FILE_NAME);
        let path = manifest_path.with_file_name(format!("{file_name}.lock"));
        let started = std::time::Instant::now();
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if started.elapsed() >= std::time::Duration::from_secs(5) {
                        anyhow::bail!(
                            "PROFILE_MANIFEST_BUSY: timed out waiting for {}",
                            path.display()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
    }
}

impl Drop for ManifestLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

fn write_manifest_atomic(path: &Path, raw: &str) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(PROFILE_FILE_NAME),
        std::process::id()
    ));
    std::fs::write(&temporary, raw).with_context(|| format!("writing {}", temporary.display()))?;
    crate::atomic_file::replace(&temporary, path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn set_manifest_field(
    manifest: &mut ApplicationProfileManifest,
    field: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    let path = |value: Option<&str>| value.map(PathBuf::from);
    let strings = |value: Option<&str>| -> anyhow::Result<Vec<String>> {
        match value {
            None => Ok(Vec::new()),
            Some(raw) if raw.trim_start().starts_with('[') => Ok(serde_json::from_str(raw)?),
            Some(raw) => Ok(raw
                .split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect()),
        }
    };
    match field {
        "id" => {
            manifest.id = value
                .ok_or_else(|| anyhow::anyhow!("id cannot be unset"))?
                .to_string()
        }
        "name" => {
            manifest.name = value
                .ok_or_else(|| anyhow::anyhow!("name cannot be unset"))?
                .to_string()
        }
        "description" => manifest.description = value.map(str::to_string),
        "version" => manifest.version = value.map(str::to_string),
        "workspace" => manifest.workspace = path(value),
        "config.runtime" => manifest.config.runtime = path(value),
        "config.channels" => manifest.config.channels = path(value),
        "resources.agents" => manifest.resources.agents = path(value),
        "resources.models" => manifest.resources.models = path(value),
        "resources.skills" => {
            manifest.resources.skills = strings(value)?.into_iter().map(PathBuf::from).collect()
        }
        "resources.workflows" => manifest.resources.workflows = path(value),
        "state.data" => manifest.state.data = path(value),
        "state.sessions" => manifest.state.sessions = path(value),
        "state.memory" => manifest.state.memory = path(value),
        "state.users" => manifest.state.users = path(value),
        "state.tasks" => manifest.state.tasks = path(value),
        "capabilities.tags" => manifest.capabilities.tags = strings(value)?,
        "capabilities.intents" => manifest.capabilities.intents = strings(value)?,
        "capabilities.channels" => manifest.capabilities.channels = strings(value)?,
        "endpoint.local.command" => {
            manifest.endpoint = ProfileEndpoint::Local {
                command: value
                    .ok_or_else(|| anyhow::anyhow!("endpoint.local.command cannot be unset"))?
                    .to_string(),
            }
        }
        "endpoint.remote.url" => {
            manifest.endpoint = ProfileEndpoint::Remote {
                url: value
                    .ok_or_else(|| anyhow::anyhow!("endpoint.remote.url cannot be unset"))?
                    .to_string(),
                auth: None,
            }
        }
        _ => anyhow::bail!(
            "unknown manifest field `{field}`; run `profile show --sources` for supported fields"
        ),
    }
    Ok(())
}

fn run_profile_resource_command(
    command: &ProfileResourceCommand,
    registry: &ProfileRegistry,
) -> anyhow::Result<()> {
    let reference = match command {
        ProfileResourceCommand::List { reference }
        | ProfileResourceCommand::Show { reference, .. }
        | ProfileResourceCommand::Check { reference } => reference,
    };
    let profile = registry.resolve(reference)?;
    match command {
        ProfileResourceCommand::List { .. } => {
            println!("RESOURCE\tRESOLVED PATH\tEXISTS");
            for (name, path) in profile_resource_paths(&profile) {
                println!("{}\t{}\t{}", name, path.display(), path.exists());
            }
        }
        ProfileResourceCommand::Show { resource, .. } => {
            let path = profile_resource_paths(&profile)
                .into_iter()
                .find(|(name, _)| name == resource)
                .map(|(_, path)| path)
                .ok_or_else(|| anyhow::anyhow!("unknown resource `{resource}`"))?;
            println!("{}", path.display());
        }
        ProfileResourceCommand::Check { .. } => check_profile(&profile, false, "plain")?,
    }
    Ok(())
}

fn profile_resource_paths(profile: &InstanceProfile) -> Vec<(String, PathBuf)> {
    let mut values = vec![
        ("config.runtime".into(), profile.runtime_config.clone()),
        ("config.channels".into(), profile.channels_config.clone()),
        ("agents".into(), profile.agents_dir.clone()),
        ("models".into(), profile.models_dir.clone()),
        ("workflows".into(), profile.workflows_dir.clone()),
        ("state.data".into(), profile.data_dir.clone()),
        ("state.sessions".into(), profile.sessions_path.clone()),
        ("state.memory".into(), profile.memory_dir.clone()),
        ("state.users".into(), profile.users_path.clone()),
        ("state.tasks".into(), profile.tasks_dir.clone()),
    ];
    values.extend(
        profile
            .skills_dirs
            .iter()
            .enumerate()
            .map(|(index, path)| (format!("skills[{index}]"), path.clone())),
    );
    values
}

fn run_profile_registry_command(
    command: &ProfileRegistryCommand,
    registry: &mut ProfileRegistry,
) -> anyhow::Result<()> {
    match command {
        ProfileRegistryCommand::Info => println!("Registry\n  Scope:       user/data-root\n  Database:    {}\n  Profiles:    {}\n  Schema:      1\n  Health:      OK", registry.path().display(), registry.entries().len()),
        ProfileRegistryCommand::List => { println!("ALIAS\tID\tMANIFEST\tREGISTERED AT"); for entry in registry.entries() { println!("@{}\t{}\t{}\t{}", entry.alias, entry.id, entry.manifest_path.display(), entry.registered_at); } }
        ProfileRegistryCommand::Repair | ProfileRegistryCommand::RebuildIndex => { let removed = registry.repair()?; println!("Registry checked. Removed {removed} stale entr{}.", if removed == 1 { "y" } else { "ies" }); }
    }
    Ok(())
}

fn profile_from_label(label: &str, data_root: &Path) -> anyhow::Result<InstanceProfile> {
    if label.ends_with(".yaml")
        || label.ends_with(".yml")
        || label.contains('/')
        || label.contains('\\')
        || Path::new(label).is_dir()
    {
        InstanceProfile::from_manifest(label)
    } else {
        InstanceProfile::from_label_in_data_root(label, data_root)
    }
}

async fn run_profile_agent_command(
    command: &ProfileAgentCommand,
    data_root: &Path,
) -> anyhow::Result<()> {
    match command {
        ProfileAgentCommand::List { profile } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile)?;
            let registry = AgentRegistry::load(&profile.agents_dir)?;
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
            ensure_profile_assets(&profile)?;
            let registry = AgentRegistry::load(&profile.agents_dir)?;
            let agent = registry
                .get(agent_id)
                .ok_or_else(|| anyhow::anyhow!("agent `{agent_id}` not found"))?;
            print_agent(agent);
        }
        ProfileAgentCommand::Upsert { profile, path } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile)?;
            let markdown = read_cli_input(path)?;
            let parsed = AgentProfile::from_markdown(&markdown)?;
            validate_file_id(&parsed.id)?;
            let agents_dir = profile.agents_dir.clone();
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
            ensure_profile_assets(&profile)?;
            let registry = AgentRegistry::load(&profile.agents_dir)?;
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
            ensure_profile_assets(&profile)?;
            println!("ID\tNAME\tNODES\tEDGES\tDESCRIPTION");
            println!("goal\tGoal\t2\t1\tEmbedded goal workflow");
            let mut workflows = load_workflow_files(&profile.workflows_dir)?;
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
            ensure_profile_assets(&profile)?;
            let workflow = if workflow_id == "goal" {
                bot_core::supervisor_workflow::embedded_goal_definition()
            } else {
                load_workflow_file(&profile.workflows_dir, workflow_id)?
            };
            print_workflow(&workflow)?;
        }
        ProfileWorkflowCommand::Upsert { profile, path } => {
            let profile = profile_from_label(profile, data_root)?;
            ensure_profile_assets(&profile)?;
            let raw = read_cli_input(path)?;
            let workflow: WorkflowDefinition =
                serde_json::from_str(&raw).context("parsing workflow JSON")?;
            workflow
                .validate()
                .map_err(|err| anyhow::anyhow!("invalid workflow: {err}"))?;
            validate_workflow_agents(&profile.agents_dir, &workflow)?;
            validate_file_id(&workflow.id)?;
            if workflow.id == "goal" {
                anyhow::bail!("embedded workflow `goal` cannot be overwritten");
            }
            let workflows_dir = profile.workflows_dir.clone();
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
            let path = profile.workflows_dir.join(format!("{workflow_id}.json"));
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

fn validate_workflow_agents(
    agents_dir: &Path,
    workflow: &WorkflowDefinition,
) -> anyhow::Result<()> {
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

fn ensure_profile_assets(profile: &InstanceProfile) -> anyhow::Result<()> {
    std::fs::create_dir_all(&profile.data_dir)?;
    install_embedded_agent_profiles(&profile.agents_dir)?;
    install_embedded_model_profiles(&profile.models_dir)?;
    std::fs::create_dir_all(&profile.workflows_dir)?;
    Ok(())
}

pub fn ensure_builtin_diagnostic_profile() -> anyhow::Result<()> {
    ensure_builtin_diagnostic_profile_in_data_root(Path::new(
        crate::instance_profile::DEFAULT_DATA_DIR,
    ))
}

pub fn ensure_builtin_diagnostic_profile_in_data_root(data_root: &Path) -> anyhow::Result<()> {
    let profile = InstanceProfile::named_in_data_root(DIAGNOSTIC_PROFILE_NAME, data_root)?;
    ensure_profile_assets(&profile)?;
    if profile.manifest_path.is_none() {
        profile.write_manifest()?;
    }
    match crate::runtime_config::detect_setup_state_at(&profile.runtime_config, &profile.data_dir) {
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
    crate::runtime_config::write_runtime_config_at(&profile.runtime_config, &config)?;
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

fn load_workflow_files(dir: &Path) -> anyhow::Result<Vec<WorkflowDefinition>> {
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

fn load_workflow_file(
    workflows_dir: &Path,
    workflow_id: &str,
) -> anyhow::Result<WorkflowDefinition> {
    validate_file_id(workflow_id)?;
    let path = workflows_dir.join(format!("{workflow_id}.json"));
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
    install_embedded_model_profiles(&profile.models_dir)?;
    install_embedded_agent_profiles(&profile.agents_dir)?;
    std::fs::create_dir_all(&profile.workflows_dir)?;

    let existing_config =
        match crate::runtime_config::detect_setup_state_at(&profile.runtime_config, data_dir) {
            SetupState::Initialized { config, .. } => Some(config),
            SetupState::Invalid { error, .. } => {
                anyhow::bail!("runtime config is invalid: {error}")
            }
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
        let registry = ModelProfileRegistry::load(&profile.models_dir)?;
        let model_profile = registry.get(&config.model_profile).ok_or_else(|| {
            anyhow::anyhow!("model profile `{}` does not exist", config.model_profile)
        })?;
        validate_model_profile_api_key(model_profile).await?;
    }
    let path = crate::runtime_config::write_runtime_config_at(&profile.runtime_config, &config)?;
    println!(
        "Saved profile `{}` runtime config to {}",
        profile.label(),
        path.display()
    );
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
        "telemetry_enabled" | "telemetry.enabled" | "telemetry" => {
            config.telemetry.enabled = parse_bool(value)?
        }
        "telemetry_agent_tracing" | "telemetry.agent_tracing" => {
            config.telemetry.agent_tracing = parse_bool(value)?
        }
        "telemetry_agent_trace_sample_rate_percent"
        | "telemetry.agent_trace_sample_rate_percent" => {
            config.telemetry.agent_trace_sample_rate_percent = parse_percent(value)?
        }
        "telemetry_capture_agent_content" | "telemetry.capture_agent_content" => {
            config.telemetry.capture_agent_content = parse_bool(value)?
        }
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
    let reserved_ports = configured_ports_in_data_root(data_dir, data_root)?;
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

fn parse_percent(value: &str) -> anyhow::Result<u8> {
    let parsed: u8 = value.parse().context("invalid percentage")?;
    if !(1..=100).contains(&parsed) {
        anyhow::bail!("percentage must be between 1 and 100");
    }
    Ok(parsed)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_with_runtime_materializes_declared_skill_directories() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("travel");
        init_profile(
            root.path(),
            directory.to_str().unwrap(),
            Some("test.travel"),
            Some("Travel"),
            "remi-cat",
            None,
            true,
        )
        .unwrap();

        let profile = InstanceProfile::from_manifest(directory.join(PROFILE_FILE_NAME)).unwrap();
        assert!(!profile.skills_dirs.is_empty());
        assert!(profile.skills_dirs.iter().all(|path| path.is_dir()));
        assert!(profile_check_issues(&profile).0.is_empty());
        assert!(profile_check_issues(&profile).1.is_empty());
    }
}
