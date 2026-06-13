//! Benchmark for precondition rule extraction at various scales
//!
//! Tests extract_timing() performance with 10, 100, 1000, and 10000 precondition rules.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rule_trigger_engine::{EvaluationContext, MonitoringEvent, Rule, TriggerConfig};

/// Generate a TriggerConfig with N precondition rules
fn generate_config_with_n_preconditions(n: usize) -> TriggerConfig {
    let preconditions: Vec<Rule> = (0..n)
        .map(|i| {
            // Rotate through different precondition types
            let rule = match i % 5 {
                0 => format!("cron('0 {} * * *')", i % 24),
                1 => "location()".to_string(),
                2 => "network_change()".to_string(),
                3 => format!("repeat_per_day({})", (i % 10) + 1),
                4 => format!("repeat_per_week({})", (i % 7) + 1),
                _ => unreachable!(),
            };
            Rule {
                rule,
                description: format!("Precondition {}", i),
            }
        })
        .collect();

    TriggerConfig {
        name: format!("Benchmark Trigger with {} preconditions", n),
        version: "v1".to_string(),
        precondition: preconditions,
        condition: vec![],
    }
}

/// Generate a TriggerConfig with N cron-only preconditions
fn generate_cron_only_config(n: usize) -> TriggerConfig {
    let preconditions: Vec<Rule> = (0..n)
        .map(|i| Rule {
            rule: format!("cron('{} {} * * *')", i % 60, i % 24),
            description: format!("Cron precondition {}", i),
        })
        .collect();

    TriggerConfig {
        name: format!("Cron-only Trigger with {} preconditions", n),
        version: "v1".to_string(),
        precondition: preconditions,
        condition: vec![],
    }
}

fn bench_extract_timing_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("extract_timing_mixed");

    for size in [10, 100, 1000, 10000].iter() {
        let config = generate_config_with_n_preconditions(*size);

        group.bench_with_input(BenchmarkId::from_parameter(size), &config, |b, config| {
            b.iter(|| {
                let result = config.extract_timing();
                black_box(result)
            });
        });
    }

    group.finish();
}

fn bench_extract_timing_cron_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("extract_timing_cron_only");

    for size in [10, 100, 1000, 10000].iter() {
        let config = generate_cron_only_config(*size);

        group.bench_with_input(BenchmarkId::from_parameter(size), &config, |b, config| {
            b.iter(|| {
                let result = config.extract_timing();
                black_box(result)
            });
        });
    }

    group.finish();
}

fn bench_single_precondition_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_precondition_type");

    // Test each precondition type individually
    let test_cases = vec![
        ("cron", "cron('0 18 * * *')"),
        ("location", "location()"),
        ("network_change", "network_change()"),
        ("repeat_per_day", "repeat_per_day(3)"),
        ("repeat_per_week", "repeat_per_week(2)"),
    ];

    for (name, rule) in test_cases {
        let config = TriggerConfig {
            name: format!("Single {} test", name),
            version: "v1".to_string(),
            precondition: vec![Rule {
                rule: rule.to_string(),
                description: format!("{} precondition", name),
            }],
            condition: vec![],
        };

        group.bench_with_input(BenchmarkId::from_parameter(name), &config, |b, config| {
            b.iter(|| {
                let result = config.extract_timing();
                black_box(result)
            });
        });
    }

    group.finish();
}

fn bench_json_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("json_parsing");

    for size in [10, 100, 1000].iter() {
        // Generate JSON string with N preconditions
        let preconditions: Vec<String> = (0..*size)
            .map(|i| {
                format!(
                    r#"{{"rule": "cron('{} {} * * *')", "description": "Precondition {}"}}"#,
                    i % 60,
                    i % 24,
                    i
                )
            })
            .collect();

        let json = format!(
            r#"{{
                "name": "JSON Parse Test",
                "precondition": [{}],
                "condition": []
            }}"#,
            preconditions.join(",\n")
        );

        group.bench_with_input(BenchmarkId::from_parameter(size), &json, |b, json| {
            b.iter(|| {
                let config = TriggerConfig::from_json(json);
                black_box(config)
            });
        });
    }

    group.finish();
}

/// Generate sample events for condition evaluation
fn generate_sample_events(count: usize) -> Vec<MonitoringEvent> {
    (0..count)
        .map(|i| MonitoringEvent {
            event_type: match i % 5 {
                0 => "connectivity".to_string(),
                1 => "location".to_string(),
                2 => "app_usage".to_string(),
                3 => "notification".to_string(),
                _ => "system".to_string(),
            },
            timestamp: (1700000000 + i as i64 * 60).to_string(), // Events 1 minute apart
            metadata_json: format!(r#"{{"index": {}, "message": "Event message {}"}}"#, i, i),
        })
        .collect()
}

/// Generate a TriggerConfig with N complex conditions
fn generate_config_with_n_conditions(n: usize) -> TriggerConfig {
    let conditions: Vec<Rule> = (0..n)
        .map(|i| {
            // Rotate through different complex condition types
            let rule = match i % 6 {
                0 => "event_count(60, 'connectivity') > 0".to_string(),
                1 => "event_count(30, 'location') >= 1 && event_count(30, 'app_usage') > 0".to_string(),
                2 => "event_exists(60, 'notification')".to_string(),
                3 => "event_exists_with_message(60, 'system', 'update')".to_string(),
                4 => "(event_count(60, '') > 5 || event_exists(30, 'connectivity')) && event_count(120, 'location') >= 0".to_string(),
                5 => "event_count(60, 'connectivity') + event_count(60, 'location') > 2".to_string(),
                _ => unreachable!(),
            };
            Rule {
                rule,
                description: format!("Complex condition {}", i),
            }
        })
        .collect();

    TriggerConfig {
        name: format!("Benchmark Trigger with {} conditions", n),
        version: "v1".to_string(),
        precondition: vec![],
        condition: conditions,
    }
}

fn bench_condition_evaluation(c: &mut Criterion) {
    let mut group = c.benchmark_group("condition_evaluation");

    // Generate 100 events for evaluation context
    let events = generate_sample_events(100);
    let current_time = 1700000000 + 100 * 60; // Time after all events

    for size in [10, 100].iter() {
        let config = generate_config_with_n_conditions(*size);

        group.bench_with_input(BenchmarkId::from_parameter(size), &config, |b, config| {
            b.iter(|| {
                let ctx = EvaluationContext {
                    events: &events,
                    current_event: None,
                    current_time,
                    timezone_offset: "+08:00",
                };
                let result = config.evaluate(&ctx);
                black_box(result)
            });
        });
    }

    group.finish();
}

fn bench_condition_with_varying_events(c: &mut Criterion) {
    let mut group = c.benchmark_group("condition_varying_events");

    // Fixed 100 conditions, vary event count
    let config = generate_config_with_n_conditions(100);

    for event_count in [10, 100, 500, 1000].iter() {
        let events = generate_sample_events(*event_count);
        let current_time = 1700000000 + (*event_count as i64) * 60;

        group.bench_with_input(
            BenchmarkId::from_parameter(event_count),
            &events,
            |b, events| {
                b.iter(|| {
                    let ctx = EvaluationContext {
                        events,
                        current_event: None,
                        current_time,
                        timezone_offset: "+08:00",
                    };
                    let result = config.evaluate(&ctx);
                    black_box(result)
                });
            },
        );
    }

    group.finish();
}

fn bench_single_complex_condition(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_complex_condition");

    let events = generate_sample_events(100);
    let current_time = 1700000000 + 100 * 60;

    let test_cases = vec![
        ("simple_count", "event_count(60, 'connectivity') > 0"),
        ("simple_exists", "event_exists(60, 'location')"),
        ("with_message", "event_exists_with_message(60, 'system', 'Event')"),
        ("and_logic", "event_count(60, 'connectivity') > 0 && event_exists(60, 'location')"),
        ("or_logic", "event_count(60, 'connectivity') > 0 || event_exists(60, 'unknown')"),
        ("complex_nested", "(event_count(60, '') > 5 || event_exists(30, 'connectivity')) && event_count(120, 'location') >= 0"),
        ("arithmetic", "event_count(60, 'connectivity') + event_count(60, 'location') + event_count(60, 'app_usage') > 10"),
    ];

    for (name, rule) in test_cases {
        let config = TriggerConfig {
            name: format!("Single {} test", name),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: rule.to_string(),
                description: format!("{} condition", name),
            }],
        };

        group.bench_with_input(BenchmarkId::from_parameter(name), &config, |b, config| {
            b.iter(|| {
                let ctx = EvaluationContext {
                    events: &events,
                    current_event: None,
                    current_time,
                    timezone_offset: "+08:00",
                };
                let result = config.evaluate(&ctx);
                black_box(result)
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_extract_timing_mixed,
    bench_extract_timing_cron_only,
    bench_single_precondition_types,
    bench_json_parsing,
    bench_condition_evaluation,
    bench_condition_with_varying_events,
    bench_single_complex_condition,
);

criterion_main!(benches);
