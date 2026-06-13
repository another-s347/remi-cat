use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{Duration, TimeZone, Utc};
use futures::executor::block_on;
use remi_client_sdk::external_tools::ExternalToolCallRequest;
use remi_client_sdk::things_crdt::{
    ContentEntry, ContentEntryPayload, ThingCollectionUpsert, ThingDatatype, ThingUpsert,
};
use remi_client_sdk::{
    EventPayload, ExternalToolExecutor, TriggerRegistration, TriggerRule, TriggerSdk,
    register_things_external_tools,
};
use serde_json::{Value as JsonValue, json};

const DEVICE_ID: &str = "bench-device";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sdk,
    Executor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    Tree,
    Ls,
    Cat,
    Edit,
    Create,
    Delete,
    Move,
    TestTrigger,
    RetrieveEvents,
    AbstractEvents,
}

#[derive(Clone, Copy)]
enum Scale {
    Small,
    Medium,
    Large,
}

#[derive(Clone, Copy)]
struct ScaleConfig {
    label: &'static str,
    collections: usize,
    things_per_collection: usize,
    child_things_per_collection: usize,
    entries_per_thing: usize,
    event_count: usize,
    trigger_count: usize,
}

const SCALE_SMALL: ScaleConfig = ScaleConfig {
    label: "small",
    collections: 3,
    things_per_collection: 6,
    child_things_per_collection: 2,
    entries_per_thing: 2,
    event_count: 120,
    trigger_count: 6,
};

const SCALE_MEDIUM: ScaleConfig = ScaleConfig {
    label: "medium",
    collections: 8,
    things_per_collection: 20,
    child_things_per_collection: 4,
    entries_per_thing: 3,
    event_count: 1_000,
    trigger_count: 24,
};

const SCALE_LARGE: ScaleConfig = ScaleConfig {
    label: "large",
    collections: 18,
    things_per_collection: 48,
    child_things_per_collection: 8,
    entries_per_thing: 4,
    event_count: 6_000,
    trigger_count: 72,
};

struct Cli {
    tool: ToolKind,
    mode: Mode,
    scale: Scale,
    iterations: usize,
    warmup: usize,
    profile_phases: bool,
    path: Option<String>,
    synthetic: bool,
    db: Option<PathBuf>,
}

#[derive(Clone)]
struct FixturePaths {
    primary_collection_uuid: String,
    secondary_collection_uuid: String,
    primary_thing_uuid: String,
    sibling_thing_uuid: String,
    thing_content_path: String,
    thing_dir_path: String,
    things_dir_path: String,
    sibling_thing_dir_path: String,
    secondary_things_dir_path: String,
}

struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new(prefix: &str) -> Result<Self> {
        let base = env::temp_dir().join("remi-sdk-benches");
        fs::create_dir_all(&base)
            .with_context(|| format!("create scratch base {}", base.display()))?;
        let unique = format!(
            "{}-{}-{}",
            prefix,
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let path = base.join(unique);
        fs::create_dir_all(&path)
            .with_context(|| format!("create scratch dir {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct FixtureState {
    _dir: ScratchDir,
    db_path: PathBuf,
    paths: FixturePaths,
    event_start: String,
    event_end: String,
    simple_trigger_json: String,
    complex_trigger_json: String,
}

fn print_help() {
    println!(
        "bench_sdk_tools\n\n  --tool <tree|ls|cat|edit|create|delete|move|test-trigger|retrieve-events|abstract-events>\n  --mode <sdk|executor>\n  --scale <small|medium|large>\n  --iterations <n>\n  --warmup <n>\n  --profile-phases\n  --path <virtual-path>\n  --synthetic\n  --db <path>\n\nExamples:\n  cargo run --bin bench_sdk_tools --release -- --tool tree --mode sdk --scale medium\n  cargo run --bin bench_sdk_tools --release -- --tool cat --mode sdk --profile-phases --scale small --synthetic\n  cargo run --bin bench_sdk_tools --release -- --tool cat --mode sdk --profile-phases --db path\\to\\remi.sqlite3 --path /collection/uuid/things/thing-uuid/content.md\n  cargo run --bin bench_sdk_tools --release -- --tool test-trigger --mode executor --scale medium\n"
    );
}

fn parse_cli() -> Result<Cli> {
    let mut tool = ToolKind::Tree;
    let mut mode = Mode::Sdk;
    let mut scale = Scale::Medium;
    let mut iterations = 30usize;
    let mut warmup = 5usize;
    let mut profile_phases = false;
    let mut path = None;
    let mut synthetic = true;
    let mut db = None;

    let args = env::args().skip(1).collect::<Vec<_>>();
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--tool" => {
                index += 1;
                let value = args.get(index).context("missing value for --tool")?;
                tool = parse_tool(value)?;
            }
            "--mode" => {
                index += 1;
                let value = args.get(index).context("missing value for --mode")?;
                mode = parse_mode(value)?;
            }
            "--scale" => {
                index += 1;
                let value = args.get(index).context("missing value for --scale")?;
                scale = parse_scale(value)?;
            }
            "--iterations" => {
                index += 1;
                let value = args.get(index).context("missing value for --iterations")?;
                iterations = value.parse().context("invalid --iterations")?;
            }
            "--warmup" => {
                index += 1;
                let value = args.get(index).context("missing value for --warmup")?;
                warmup = value.parse().context("invalid --warmup")?;
            }
            "--profile-phases" => {
                profile_phases = true;
            }
            "--path" => {
                index += 1;
                let value = args.get(index).context("missing value for --path")?;
                path = Some(value.to_string());
            }
            "--synthetic" => {
                synthetic = true;
            }
            "--db" => {
                index += 1;
                let value = args.get(index).context("missing value for --db")?;
                db = Some(PathBuf::from(value));
                synthetic = false;
            }
            other => bail!("unknown argument: {other}"),
        }
        index += 1;
    }

    Ok(Cli {
        tool,
        mode,
        scale,
        iterations,
        warmup,
        profile_phases,
        path,
        synthetic,
        db,
    })
}

fn parse_tool(value: &str) -> Result<ToolKind> {
    match value {
        "tree" => Ok(ToolKind::Tree),
        "ls" => Ok(ToolKind::Ls),
        "cat" => Ok(ToolKind::Cat),
        "edit" => Ok(ToolKind::Edit),
        "create" => Ok(ToolKind::Create),
        "delete" => Ok(ToolKind::Delete),
        "move" => Ok(ToolKind::Move),
        "test-trigger" => Ok(ToolKind::TestTrigger),
        "retrieve-events" => Ok(ToolKind::RetrieveEvents),
        "abstract-events" => Ok(ToolKind::AbstractEvents),
        other => bail!("unsupported tool: {other}"),
    }
}

fn parse_mode(value: &str) -> Result<Mode> {
    match value {
        "sdk" => Ok(Mode::Sdk),
        "executor" => Ok(Mode::Executor),
        other => bail!("unsupported mode: {other}"),
    }
}

fn parse_scale(value: &str) -> Result<Scale> {
    match value {
        "small" => Ok(Scale::Small),
        "medium" => Ok(Scale::Medium),
        "large" => Ok(Scale::Large),
        other => bail!("unsupported scale: {other}"),
    }
}

fn scale_config(scale: Scale) -> ScaleConfig {
    match scale {
        Scale::Small => SCALE_SMALL,
        Scale::Medium => SCALE_MEDIUM,
        Scale::Large => SCALE_LARGE,
    }
}

fn build_registration(trigger_uuid: &str, index: usize) -> TriggerRegistration {
    TriggerRegistration {
        trigger_uuid: trigger_uuid.to_string(),
        name: format!("Trigger {index}"),
        version: "1.0".to_string(),
        precondition: vec![TriggerRule {
            rule: format!("cron('{} {} * * *')", index % 60, (index / 2) % 24),
            description: "cron".to_string(),
        }],
        condition: Vec::new(),
        action_uuid: None,
        action_args: json!({}),
    }
}

fn build_trigger_json(name: &str, condition_count: usize) -> String {
    let conditions = (0..condition_count)
        .map(|index| match index % 4 {
            0 => json!({
                "rule": "event_count(120, 'Connectivity') > 0",
                "description": "recent connectivity"
            }),
            1 => json!({
                "rule": "event_count(120, 'Location') > 0",
                "description": "recent location"
            }),
            2 => json!({
                "rule": "event_exists(600, 'Connectivity')",
                "description": "connectivity exists"
            }),
            _ => json!({
                "rule": "event_exists(600, 'Location')",
                "description": "location exists"
            }),
        })
        .collect::<Vec<_>>();

    json!({
        "name": name,
        "version": "1.0",
        "precondition": [
            {
                "rule": "cron('0 9 * * *')",
                "description": "daily"
            }
        ],
        "condition": conditions,
    })
    .to_string()
}

fn seed_virtual_fs_fixture(sdk: &TriggerSdk, scale: ScaleConfig) -> Result<FixturePaths> {
    for collection_index in 0..scale.collections {
        let collection_uuid = format!("c{collection_index:02}");
        sdk.things_upsert_collection(
            DEVICE_ID,
            ThingCollectionUpsert {
                uuid: collection_uuid.clone(),
                title: format!("Collection {collection_index}"),
                trigger_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )?;

        for thing_index in 0..scale.things_per_collection {
            let thing_uuid = format!("c{collection_index:02}t{thing_index:03}");
            sdk.things_upsert_thing(
                DEVICE_ID,
                ThingUpsert {
                    uuid: thing_uuid.clone(),
                    title: format!("Thing {collection_index}-{thing_index}"),
                    datatype: ThingDatatype::Markdown,
                    data: Some(json!({
                        "markdown": format!("# Thing {collection_index}-{thing_index}\n\nseed content")
                    })),
                    collection_uuid: collection_uuid.clone(),
                    trigger_uuid: None,
                    parent_uuid: None,
                    created_at: None,
                    updated_at: None,
                },
            )?;

            for entry_index in 0..scale.entries_per_thing {
                sdk.things_add_content_entry(
                    DEVICE_ID,
                    &thing_uuid,
                    ContentEntry {
                        id: format!("{thing_uuid}-entry-{entry_index}"),
                        title: Some(format!("Entry {entry_index}")),
                        order: entry_index as f64,
                        payload: ContentEntryPayload::Custom {
                            content_type: "benchmark-note".to_string(),
                            data: json!({
                                "thing": thing_uuid,
                                "index": entry_index,
                            }),
                        },
                    },
                )?;
            }
        }

        let parent_uuid = format!("c{collection_index:02}t000");
        for child_index in 0..scale.child_things_per_collection {
            let thing_uuid = format!("c{collection_index:02}child{child_index:03}");
            sdk.things_upsert_thing(
                DEVICE_ID,
                ThingUpsert {
                    uuid: thing_uuid,
                    title: format!("Child {collection_index}-{child_index}"),
                    datatype: ThingDatatype::Markdown,
                    data: Some(json!({"markdown": "child"})),
                    collection_uuid: collection_uuid.clone(),
                    trigger_uuid: None,
                    parent_uuid: Some(parent_uuid.clone()),
                    created_at: None,
                    updated_at: None,
                },
            )?;
        }
    }

    for trigger_index in 0..scale.trigger_count {
        let trigger_uuid = format!("tr-{trigger_index:03}");
        sdk.register_trigger(build_registration(&trigger_uuid, trigger_index))?;
    }

    let primary_collection_uuid = "c00".to_string();
    let secondary_collection_uuid = if scale.collections > 1 {
        "c01".to_string()
    } else {
        "c00".to_string()
    };
    let primary_thing_uuid = "c00t000".to_string();
    let sibling_thing_uuid = if scale.things_per_collection > 1 {
        "c00t001".to_string()
    } else {
        "c00t000".to_string()
    };

    sdk.things_set_collection_trigger_uuid(DEVICE_ID, &primary_collection_uuid, Some("tr-000"))?;
    sdk.upsert_trigger_binding("tr-000", "collection", &primary_collection_uuid)?;
    sdk.things_set_thing_trigger_uuid(DEVICE_ID, &primary_thing_uuid, Some("tr-001"))?;
    sdk.upsert_trigger_binding("tr-001", "thing", &primary_thing_uuid)?;

    Ok(FixturePaths {
        primary_collection_uuid: primary_collection_uuid.clone(),
        secondary_collection_uuid: secondary_collection_uuid.clone(),
        primary_thing_uuid: primary_thing_uuid.clone(),
        sibling_thing_uuid: sibling_thing_uuid.clone(),
        thing_content_path: format!(
            "/collection/{primary_collection_uuid}/things/{primary_thing_uuid}/content.md"
        ),
        thing_dir_path: format!(
            "/collection/{primary_collection_uuid}/things/{primary_thing_uuid}"
        ),
        things_dir_path: format!("/collection/{primary_collection_uuid}/things"),
        sibling_thing_dir_path: format!(
            "/collection/{primary_collection_uuid}/things/{sibling_thing_uuid}"
        ),
        secondary_things_dir_path: format!("/collection/{secondary_collection_uuid}/things"),
    })
}

fn seed_events(sdk: &TriggerSdk, count: usize) -> Result<(String, String)> {
    let start = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("fixed benchmark timestamp");

    for index in 0..count {
        sdk.record_event(EventPayload {
            event_type: if index % 2 == 0 {
                "Connectivity".to_string()
            } else {
                "Location".to_string()
            },
            timestamp: start + Duration::minutes(index as i64),
            metadata: json!({
                "index": index,
                "message": format!("event-{index}"),
            }),
        })?;
    }

    let (event_start, event_end) = sdk
        .event_time_range()?
        .context("event range missing after seed")?;
    Ok((event_start.to_rfc3339(), event_end.to_rfc3339()))
}

fn build_synthetic_fixture(scale: Scale) -> Result<FixtureState> {
    let config = scale_config(scale);
    let dir = ScratchDir::new("synthetic")?;
    let db_path = dir.path().join("bench-sdk-tools.sqlite3");
    let sdk = TriggerSdk::initialize(&db_path).context("init sdk")?;
    let paths = seed_virtual_fs_fixture(&sdk, config)?;
    let (event_start, event_end) = seed_events(&sdk, config.event_count)?;
    Ok(FixtureState {
        _dir: dir,
        db_path,
        paths,
        event_start,
        event_end,
        simple_trigger_json: build_trigger_json("Simple Trigger", 1),
        complex_trigger_json: build_trigger_json("Complex Trigger", 8),
    })
}

fn copy_database(source: &Path) -> Result<(ScratchDir, PathBuf)> {
    let dir = ScratchDir::new("iteration")?;
    let target = dir.path().join("bench-run.sqlite3");
    fs::copy(source, &target)
        .with_context(|| format!("copy {} -> {}", source.display(), target.display()))?;
    Ok((dir, target))
}

fn build_executor(sdk: Arc<TriggerSdk>) -> ExternalToolExecutor {
    let mut executor = ExternalToolExecutor::new();
    register_things_external_tools(&mut executor, sdk, DEVICE_ID);
    executor
}

fn summarize(mut samples: Vec<StdDuration>) -> String {
    if samples.is_empty() {
        return "n=0".to_string();
    }

    samples.sort();
    let n = samples.len();
    let sum = samples
        .iter()
        .copied()
        .fold(StdDuration::from_millis(0), |acc, sample| acc + sample);
    let mean = sum / (n as u32);
    let median = samples[n / 2];
    let p90 = samples[((n as f64) * 0.90).floor().min((n - 1) as f64) as usize];
    let min = samples[0];
    let max = samples[n - 1];

    format!(
        "n={} mean_ms={} median_ms={} p90_ms={} min_ms={} max_ms={}",
        n,
        mean.as_millis(),
        median.as_millis(),
        p90.as_millis(),
        min.as_millis(),
        max.as_millis()
    )
}

fn run_once(
    tool: ToolKind,
    mode: Mode,
    fixture: &FixtureState,
    db_path: &Path,
    path_override: Option<&str>,
) -> Result<JsonValue> {
    let sdk = Arc::new(
        TriggerSdk::initialize(db_path)
            .with_context(|| format!("init sdk on {}", db_path.display()))?,
    );

    match mode {
        Mode::Sdk => run_sdk_tool(tool, sdk.as_ref(), fixture, path_override),
        Mode::Executor => run_executor_tool(tool, sdk, fixture),
    }
}

fn run_sdk_tool(
    tool: ToolKind,
    sdk: &TriggerSdk,
    fixture: &FixtureState,
    path_override: Option<&str>,
) -> Result<JsonValue> {
    let tree_path = path_override;
    let cat_path = path_override.unwrap_or(&fixture.paths.thing_content_path);
    let ls_path = path_override.unwrap_or(&fixture.paths.things_dir_path);

    match tool {
        ToolKind::Tree => Ok(json!(sdk.tree_virtual_path(DEVICE_ID, tree_path)?)),
        ToolKind::Ls => Ok(json!(sdk.ls_virtual_path(DEVICE_ID, Some(ls_path))?)),
        ToolKind::Cat => Ok(serde_json::to_value(
            sdk.read_virtual_path(DEVICE_ID, cat_path)?,
        )?),
        ToolKind::Edit => Ok(sdk.edit_virtual_path(
            DEVICE_ID,
            cat_path,
            "append",
            Some(&json!("\nbench-update")),
            None,
            None,
            None,
        )?),
        ToolKind::Create => Ok(sdk.create_virtual_path(
            DEVICE_ID,
            &fixture.paths.things_dir_path,
            "thing",
            None,
            Some("CLI created thing"),
            Some("seed body"),
            None,
            None,
            None,
        )?),
        ToolKind::Delete => {
            Ok(sdk.delete_virtual_path(DEVICE_ID, &fixture.paths.sibling_thing_dir_path)?)
        }
        ToolKind::Move => Ok(sdk.move_virtual_path(
            DEVICE_ID,
            &fixture.paths.sibling_thing_dir_path,
            &fixture.paths.secondary_things_dir_path,
        )?),
        ToolKind::TestTrigger => Ok(json!(sdk.trigger_test_json(
            &fixture.complex_trigger_json,
            None,
            None,
            true,
        )?)),
        ToolKind::RetrieveEvents => Ok(json!(
            sdk.events_list_between_json(&fixture.event_start, &fixture.event_end,)?
        )),
        ToolKind::AbstractEvents => Ok(json!(sdk.events_abstract_json(3)?)),
    }
}

fn run_executor_tool(
    tool: ToolKind,
    sdk: Arc<TriggerSdk>,
    fixture: &FixtureState,
) -> Result<JsonValue> {
    let executor = build_executor(sdk);
    let (tool_name, arguments) = match tool {
        ToolKind::Tree => ("tree_tool", json!({})),
        ToolKind::Ls => ("ls_tool", json!({"path": fixture.paths.things_dir_path})),
        ToolKind::Cat => (
            "cat_tool",
            json!({"path": fixture.paths.thing_content_path}),
        ),
        ToolKind::Edit => (
            "edit_path_tool",
            json!({
                "path": fixture.paths.thing_content_path,
                "operation": "append",
                "value": "\nbench-update",
            }),
        ),
        ToolKind::Create => (
            "create_tool",
            json!({
                "parent_path": fixture.paths.things_dir_path,
                "type_name": "thing",
                "title": "CLI created thing",
                "content": "seed body",
            }),
        ),
        ToolKind::Delete => (
            "delete_path_tool",
            json!({"path": fixture.paths.sibling_thing_dir_path}),
        ),
        ToolKind::Move => (
            "move_path_tool",
            json!({
                "from_path": fixture.paths.sibling_thing_dir_path,
                "to_path": fixture.paths.secondary_things_dir_path,
            }),
        ),
        ToolKind::TestTrigger => (
            "test_trigger",
            json!({"trigger": fixture.complex_trigger_json}),
        ),
        ToolKind::RetrieveEvents => (
            "retrieve_events",
            json!({
                "start_time": fixture.event_start,
                "end_time": fixture.event_end,
            }),
        ),
        ToolKind::AbstractEvents => ("abstract_events", json!({"top_n": 3})),
    };

    let plan = block_on(executor.resolve_calls(vec![ExternalToolCallRequest {
        tool_call_id: "bench-call".to_string(),
        tool_name: tool_name.to_string(),
        arguments,
    }]));

    Ok(json!({
        "resolved": plan.resolved_results.len(),
        "pending": plan.pending_calls.len(),
        "things_changed": plan.things_changed,
        "trigger_scheduler_sync_needed": plan.trigger_scheduler_sync_needed,
    }))
}

fn main() -> Result<()> {
    let cli = parse_cli()?;
    if cli.synthetic {
        eprintln!(
            "Building synthetic fixture at scale={} for tool={:?}...",
            scale_config(cli.scale).label,
            cli.tool,
        );
    }
    let fixture = if cli.synthetic {
        build_synthetic_fixture(cli.scale)?
    } else {
        let db_path = cli
            .db
            .as_ref()
            .context("--db is required when --synthetic is not used")?;
        let dir = ScratchDir::new("external-db")?;
        FixtureState {
            _dir: dir,
            db_path: db_path.clone(),
            paths: FixturePaths {
                primary_collection_uuid: "c00".to_string(),
                secondary_collection_uuid: "c01".to_string(),
                primary_thing_uuid: "c00t000".to_string(),
                sibling_thing_uuid: "c00t001".to_string(),
                thing_content_path: "/collection/c00/things/c00t000/content.md".to_string(),
                thing_dir_path: "/collection/c00/things/c00t000".to_string(),
                things_dir_path: "/collection/c00/things".to_string(),
                sibling_thing_dir_path: "/collection/c00/things/c00t001".to_string(),
                secondary_things_dir_path: "/collection/c01/things".to_string(),
            },
            event_start: String::new(),
            event_end: String::new(),
            simple_trigger_json: build_trigger_json("Simple Trigger", 1),
            complex_trigger_json: build_trigger_json("Complex Trigger", 8),
        }
    };

    eprintln!(
        "Fixture ready. mode={:?} tool={:?} source={}{}",
        cli.mode,
        cli.tool,
        if cli.synthetic { "synthetic" } else { "db" },
        cli.path
            .as_ref()
            .map(|value| format!(" path={value}"))
            .unwrap_or_default(),
    );

    let mut samples = Vec::with_capacity(cli.iterations);
    let template_db_path = fixture.db_path.clone();
    let needs_copy = matches!(
        cli.tool,
        ToolKind::Edit | ToolKind::Create | ToolKind::Delete | ToolKind::Move
    );

    if cli.profile_phases {
        if cli.mode != Mode::Sdk {
            bail!("--profile-phases currently only supports --mode sdk");
        }

        eprintln!("Collecting phase timings...");
        let sdk = TriggerSdk::initialize(&template_db_path)
            .with_context(|| format!("init sdk on {}", template_db_path.display()))?;
        let profile = match cli.tool {
            ToolKind::Tree => sdk.profile_tree_virtual_path(DEVICE_ID, cli.path.as_deref())?,
            ToolKind::Cat => sdk.profile_read_virtual_path(
                DEVICE_ID,
                cli.path
                    .as_deref()
                    .unwrap_or(&fixture.paths.thing_content_path),
            )?,
            other => bail!(
                "--profile-phases currently supports only tree and cat, got {:?}",
                other
            ),
        };
        println!("{}", serde_json::to_string_pretty(&profile)?);
        return Ok(());
    }

    for _ in 0..cli.warmup {
        if needs_copy {
            let (_dir, db_path) = copy_database(&template_db_path)?;
            let _ = run_once(cli.tool, cli.mode, &fixture, &db_path, cli.path.as_deref())?;
        } else {
            let _ = run_once(
                cli.tool,
                cli.mode,
                &fixture,
                &template_db_path,
                cli.path.as_deref(),
            )?;
        }
    }

    for _ in 0..cli.iterations {
        let started = Instant::now();
        if needs_copy {
            let (_dir, db_path) = copy_database(&template_db_path)?;
            let _ = run_once(cli.tool, cli.mode, &fixture, &db_path, cli.path.as_deref())?;
        } else {
            let _ = run_once(
                cli.tool,
                cli.mode,
                &fixture,
                &template_db_path,
                cli.path.as_deref(),
            )?;
        }
        samples.push(started.elapsed());
    }

    println!(
        "tool={:?} mode={:?} scale={} source={} {}",
        cli.tool,
        cli.mode,
        scale_config(cli.scale).label,
        if cli.synthetic { "synthetic" } else { "db" },
        summarize(samples),
    );

    Ok(())
}
