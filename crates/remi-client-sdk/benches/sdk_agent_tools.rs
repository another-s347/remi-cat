use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{Duration, TimeZone, Utc};
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use futures::executor::block_on;
use remi_client_sdk::external_tools::ExternalToolCallRequest;
use remi_client_sdk::things_crdt::{
    ContentEntry, ContentEntryPayload, ThingCollectionUpsert, ThingDatatype, ThingUpsert,
};
use remi_client_sdk::things_handlers::{
    EventsAbstractRequestHandler, EventsRetrieveRequestHandler, TriggerTestRequestHandler,
    VirtualFsCatHandler,
};
use remi_client_sdk::{
    EventPayload, ExternalToolExecutor, ExternalToolHandler, TriggerRegistration, TriggerRule,
    TriggerSdk, register_things_external_tools,
};
use serde_json::json;
use tempfile::{TempDir, tempdir};

const DEVICE_ID: &str = "bench-device";

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

struct BenchFixture {
    _dir: TempDir,
    sdk: Arc<TriggerSdk>,
    paths: FixturePaths,
    event_start: String,
    event_end: String,
    simple_trigger_json: String,
    complex_trigger_json: String,
}

fn init_sdk() -> Result<(TempDir, Arc<TriggerSdk>)> {
    let dir = tempdir().context("tempdir")?;
    let db_path = dir.path().join("sdk-agent-tools-bench.sqlite3");
    let sdk = Arc::new(TriggerSdk::initialize(&db_path).context("init sdk")?);
    Ok((dir, sdk))
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
    }
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

fn build_fixture(scale: ScaleConfig) -> Result<BenchFixture> {
    let (dir, sdk) = init_sdk()?;
    let paths = seed_virtual_fs_fixture(sdk.as_ref(), scale)?;
    let (event_start, event_end) = seed_events(sdk.as_ref(), scale.event_count)?;
    Ok(BenchFixture {
        _dir: dir,
        sdk,
        paths,
        event_start,
        event_end,
        simple_trigger_json: build_trigger_json("Simple Trigger", 1),
        complex_trigger_json: build_trigger_json("Complex Trigger", 8),
    })
}

fn build_executor(sdk: Arc<TriggerSdk>) -> ExternalToolExecutor {
    let mut executor = ExternalToolExecutor::new();
    register_things_external_tools(&mut executor, sdk, DEVICE_ID);
    executor
}

fn bench_virtual_fs(c: &mut Criterion) {
    let mut group = c.benchmark_group("virtual_fs");

    for scale in [SCALE_SMALL, SCALE_MEDIUM, SCALE_LARGE] {
        let fixture = build_fixture(scale).expect("virtual fs fixture");
        let cat_handler = VirtualFsCatHandler::new(fixture.sdk.clone(), DEVICE_ID.to_string());
        let executor = build_executor(fixture.sdk.clone());
        let content_path = fixture.paths.thing_content_path.clone();

        group.bench_with_input(
            BenchmarkId::new("tree_sdk", scale.label),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    black_box(
                        fixture
                            .sdk
                            .tree_virtual_path(DEVICE_ID, None)
                            .expect("tree virtual path"),
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("cat_sdk", scale.label),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    black_box(
                        fixture
                            .sdk
                            .read_virtual_path(DEVICE_ID, &content_path)
                            .expect("read virtual path"),
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("cat_handler", scale.label),
            &fixture,
            |b, _fixture| {
                b.iter(|| {
                    black_box(
                        block_on(cat_handler.handle(
                            "bench-cat",
                            &json!({
                                "path": content_path,
                            }),
                        ))
                        .expect("cat handler"),
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("tree_executor", scale.label),
            &fixture,
            |b, _fixture| {
                b.iter(|| {
                    black_box(block_on(executor.resolve_calls(vec![
                        ExternalToolCallRequest {
                            tool_call_id: "tree-call".to_string(),
                            tool_name: "tree_tool".to_string(),
                            arguments: json!({}),
                        },
                    ])))
                });
            },
        );
    }

    group.finish();
}

fn bench_events(c: &mut Criterion) {
    let mut group = c.benchmark_group("events");

    for scale in [SCALE_SMALL, SCALE_MEDIUM, SCALE_LARGE] {
        let fixture = build_fixture(scale).expect("events fixture");
        let retrieve_handler = EventsRetrieveRequestHandler::new(fixture.sdk.clone());
        let abstract_handler = EventsAbstractRequestHandler::new(fixture.sdk.clone());
        let executor = build_executor(fixture.sdk.clone());
        let start_time = fixture.event_start.clone();
        let end_time = fixture.event_end.clone();

        group.bench_with_input(
            BenchmarkId::new("retrieve_sdk", scale.label),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    black_box(
                        fixture
                            .sdk
                            .events_list_between_json(&start_time, &end_time)
                            .expect("retrieve events"),
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("abstract_sdk", scale.label),
            &fixture,
            |b, fixture| {
                b.iter(|| {
                    black_box(
                        fixture
                            .sdk
                            .events_abstract_json(3)
                            .expect("abstract events"),
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("retrieve_handler", scale.label),
            &fixture,
            |b, _fixture| {
                b.iter(|| {
                    black_box(
                        block_on(retrieve_handler.handle(
                            "retrieve-events",
                            &json!({
                                "start_time": start_time,
                                "end_time": end_time,
                            }),
                        ))
                        .expect("retrieve handler"),
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("abstract_executor", scale.label),
            &fixture,
            |b, _fixture| {
                b.iter(|| {
                    black_box(block_on(executor.resolve_calls(vec![
                        ExternalToolCallRequest {
                            tool_call_id: "abstract-call".to_string(),
                            tool_name: "abstract_events".to_string(),
                            arguments: json!({"top_n": 3}),
                        },
                    ])))
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("abstract_handler", scale.label),
            &fixture,
            |b, _fixture| {
                b.iter(|| {
                    black_box(
                        block_on(abstract_handler.handle("abstract-events", &json!({"top_n": 3})))
                            .expect("abstract handler"),
                    )
                });
            },
        );
    }

    group.finish();
}

fn bench_trigger(c: &mut Criterion) {
    let mut group = c.benchmark_group("trigger_test");
    let fixture = build_fixture(SCALE_MEDIUM).expect("trigger fixture");
    let handler = TriggerTestRequestHandler::new(fixture.sdk.clone());
    let executor = build_executor(fixture.sdk.clone());

    for (label, trigger_json) in [
        ("simple", fixture.simple_trigger_json.clone()),
        ("complex", fixture.complex_trigger_json.clone()),
    ] {
        group.bench_with_input(
            BenchmarkId::new("sdk", label),
            &trigger_json,
            |b, trigger_json| {
                b.iter(|| {
                    black_box(
                        fixture
                            .sdk
                            .trigger_test_json(trigger_json, None, None, true)
                            .expect("trigger test"),
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("handler", label),
            &trigger_json,
            |b, trigger_json| {
                b.iter(|| {
                    black_box(
                        block_on(handler.handle(
                            "trigger-handler",
                            &json!({
                                "trigger_json": trigger_json,
                                "manual": true,
                            }),
                        ))
                        .expect("trigger handler"),
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("executor", label),
            &trigger_json,
            |b, trigger_json| {
                b.iter(|| {
                    black_box(block_on(executor.resolve_calls(vec![
                        ExternalToolCallRequest {
                            tool_call_id: "trigger-call".to_string(),
                            tool_name: "test_trigger".to_string(),
                            arguments: json!({
                                "trigger": trigger_json,
                            }),
                        },
                    ])))
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    sdk_agent_tools,
    bench_virtual_fs,
    bench_events,
    bench_trigger
);
criterion_main!(sdk_agent_tools);
