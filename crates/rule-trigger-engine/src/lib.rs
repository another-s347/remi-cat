use cel::{Context, Program};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDateTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;

// Shared trigger schema and CEL evaluation helpers live in this crate.

/// Represents a monitoring event from the host.
/// This is a generic event struct - concrete event types (schemas, validation)
/// are defined on the application side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringEvent {
    /// Event type identifier (e.g., "Connectivity", "Location", "System")
    pub event_type: String,
    /// ISO8601 timestamp string
    pub timestamp: String,
    /// Event metadata as JSON string - structure is defined by event type
    pub metadata_json: String,
}

/// Timing types that can be returned by precondition functions
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerTiming {
    /// Cron-based scheduling using POSIX cron format (5 fields)
    ///
    /// Format: `minute hour day-of-month month day-of-week`
    ///
    /// Field ranges:
    /// - minute: 0-59
    /// - hour: 0-23
    /// - day-of-month: 1-31
    /// - month: 1-12
    /// - day-of-week: 0-6 (0 = Sunday)
    ///
    /// Special characters:
    /// - `*` matches any value
    /// - `,` separates multiple values (e.g., `1,3,5`)
    /// - `-` defines a range (e.g., `1-5`)
    /// - `/` defines step values (e.g., `*/15` for every 15 units)
    ///
    /// Examples:
    /// - `0 9 * * *` - Daily at 9:00 AM
    /// - `30 18 * * 1-5` - Weekdays at 6:30 PM
    /// - `0 9 * * 0` - Every Sunday at 9:00 AM
    /// - `0 */2 * * *` - Every 2 hours
    /// - `0 9 1 * *` - First day of every month at 9:00 AM
    Cron { expression: String },
    /// One-shot timer trigger. The literal is resolved downstream against a registration anchor.
    Timer { value: String },
    /// Event-based reactive trigger.
    Event { event_type: String },
    /// Repeat frequency trigger
    RepeatFrequency { frequency: RepeatFrequency },
}

/// Repeat frequency options
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatFrequency {
    PerDay(u32),
    PerWeek(u32),
}

/// Root configuration for a rule-based trigger using CEL
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerConfig {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Preconditions determine when the trigger should be called (replaces timing)
    pub precondition: Vec<Rule>,
    /// Conditions determine whether to notify the user (replaces run logic)
    pub condition: Vec<Rule>,
}

fn default_version() -> String {
    "v1".to_string()
}

/// A rule with CEL expression and description
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// CEL expression
    pub rule: String,
    /// Human-readable description
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreconditionPolicy {
    /// Preconditions are treated as boolean gates.
    /// If any gate precondition evaluates to false/error, conditions are skipped.
    EnforceAsGates,
    /// Preconditions are evaluated and logged but do not gate condition execution.
    IgnoreGates,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEvalOutcome {
    True,
    False,
    Timing,
    NonBool,
    Error,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleEvaluation {
    pub index: u32,
    pub description: String,
    pub rule: String,
    pub outcome: RuleEvalOutcome,
    pub bool_value: Option<bool>,
    pub raw_value: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerEvaluationReport {
    pub precondition_policy: PreconditionPolicy,
    pub precondition_gate_passed: bool,
    pub conditions_passed: bool,
    pub overall_result: bool,
    pub preconditions: Vec<RuleEvaluation>,
    pub conditions: Vec<RuleEvaluation>,
}

/// Alias for compatibility with tests
pub type TriggerRule = Rule;

/// Context for rule evaluation
pub struct EvaluationContext<'a> {
    pub events: &'a [MonitoringEvent],
    pub current_event: Option<&'a MonitoringEvent>,
    pub current_time: i64, // Unix timestamp
    pub timezone_offset: &'a str,
}

impl TriggerConfig {
    /// Load configuration from JSON string
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("Failed to parse JSON: {}", e))
    }

    /// Evaluate the trigger conditions against the given context
    /// Returns true if all conditions pass (AND logic)
    pub fn evaluate(&self, eval_ctx: &EvaluationContext) -> Result<bool, String> {
        // Create CEL context with helper functions
        let context = create_cel_context(eval_ctx);

        // Evaluate all conditions - all must be true (AND logic)
        for condition in &self.condition {
            let program = Program::compile(&condition.rule).map_err(|e| {
                format!(
                    "Failed to compile CEL expression '{}': {}",
                    condition.description, e
                )
            })?;

            let value = program.execute(&context).map_err(|e| {
                format!(
                    "Failed to execute CEL expression '{}': {}",
                    condition.description, e
                )
            })?;

            match value {
                cel::Value::Bool(b) => {
                    if !b {
                        return Ok(false); // Short-circuit on first false
                    }
                }
                _ => {
                    return Err(format!(
                        "CEL expression '{}' must return boolean, got: {:?}",
                        condition.description, value
                    ))
                }
            }
        }

        Ok(true) // All conditions passed
    }

    /// Evaluate preconditions + conditions and return a detailed report.
    ///
    /// - Preconditions can include both timing rules (cron/timer/event/repeat) and boolean gate rules.
    /// - Gate preconditions are only those that are *not* timing rules and must return boolean.
    /// - Conditions must return boolean and are evaluated with AND + short-circuit.
    pub fn evaluate_detailed(
        &self,
        eval_ctx: &EvaluationContext,
        precondition_policy: PreconditionPolicy,
    ) -> TriggerEvaluationReport {
        let condition_context = create_cel_context(eval_ctx);
        let timing_context = create_precondition_context();

        let mut preconditions = Vec::with_capacity(self.precondition.len());
        let mut gate_passed = true;

        for (idx, precond) in self.precondition.iter().enumerate() {
            let is_timing = is_timing_precondition(&precond.rule);

            if is_timing {
                // Timing preconditions are evaluated in the timing-only context.
                // They are logged but do not participate in gating.
                let eval = match Program::compile(&precond.rule)
                    .map_err(|e| {
                        format!(
                            "Failed to compile precondition '{}': {e}",
                            precond.description
                        )
                    })
                    .and_then(|p| {
                        p.execute(&timing_context).map_err(|e| {
                            format!(
                                "Failed to execute precondition '{}': {e}",
                                precond.description
                            )
                        })
                    }) {
                    Ok(value) => {
                        let raw = Some(cel_value_to_json(&value));
                        let outcome = if parse_timing_value(&value).is_some() {
                            RuleEvalOutcome::Timing
                        } else {
                            RuleEvalOutcome::NonBool
                        };
                        RuleEvaluation {
                            index: idx as u32,
                            description: precond.description.clone(),
                            rule: precond.rule.clone(),
                            outcome,
                            bool_value: None,
                            raw_value: raw,
                            error: None,
                        }
                    }
                    Err(err) => RuleEvaluation {
                        index: idx as u32,
                        description: precond.description.clone(),
                        rule: precond.rule.clone(),
                        outcome: RuleEvalOutcome::Error,
                        bool_value: None,
                        raw_value: None,
                        error: Some(format!("{err}")),
                    },
                };

                preconditions.push(eval);
                continue;
            }

            // Gate preconditions must return boolean and are evaluated in the full context.
            let eval = match Program::compile(&precond.rule)
                .map_err(|e| {
                    format!(
                        "Failed to compile precondition '{}': {e}",
                        precond.description
                    )
                })
                .and_then(|p| {
                    p.execute(&condition_context).map_err(|e| {
                        format!(
                            "Failed to execute precondition '{}': {e}",
                            precond.description
                        )
                    })
                }) {
                Ok(value) => match value {
                    cel::Value::Bool(b) => {
                        if !b {
                            gate_passed = false;
                        }
                        RuleEvaluation {
                            index: idx as u32,
                            description: precond.description.clone(),
                            rule: precond.rule.clone(),
                            outcome: if b {
                                RuleEvalOutcome::True
                            } else {
                                RuleEvalOutcome::False
                            },
                            bool_value: Some(b),
                            raw_value: Some(serde_json::Value::Bool(b)),
                            error: None,
                        }
                    }
                    other => {
                        gate_passed = false;
                        RuleEvaluation {
                            index: idx as u32,
                            description: precond.description.clone(),
                            rule: precond.rule.clone(),
                            outcome: RuleEvalOutcome::NonBool,
                            bool_value: None,
                            raw_value: Some(cel_value_to_json(&other)),
                            error: Some(format!(
                                "Precondition must return boolean for gating, got: {:?}",
                                other
                            )),
                        }
                    }
                },
                Err(err) => {
                    gate_passed = false;
                    RuleEvaluation {
                        index: idx as u32,
                        description: precond.description.clone(),
                        rule: precond.rule.clone(),
                        outcome: RuleEvalOutcome::Error,
                        bool_value: None,
                        raw_value: None,
                        error: Some(format!("{err}")),
                    }
                }
            };

            preconditions.push(eval);
        }

        let should_skip_conditions =
            precondition_policy == PreconditionPolicy::EnforceAsGates && !gate_passed;

        let mut conditions = Vec::with_capacity(self.condition.len());
        let mut conditions_passed = true;

        if should_skip_conditions {
            for (idx, condition) in self.condition.iter().enumerate() {
                conditions.push(RuleEvaluation {
                    index: idx as u32,
                    description: condition.description.clone(),
                    rule: condition.rule.clone(),
                    outcome: RuleEvalOutcome::Skipped,
                    bool_value: None,
                    raw_value: None,
                    error: None,
                });
            }
            conditions_passed = false;
        } else {
            let mut stopped = false;
            for (idx, condition) in self.condition.iter().enumerate() {
                if stopped {
                    conditions.push(RuleEvaluation {
                        index: idx as u32,
                        description: condition.description.clone(),
                        rule: condition.rule.clone(),
                        outcome: RuleEvalOutcome::Skipped,
                        bool_value: None,
                        raw_value: None,
                        error: None,
                    });
                    continue;
                }

                let is_timing = is_timing_precondition(&condition.rule);
                if is_timing {
                    // Timing helpers (e.g. repeat_per_day/week) may live in `condition` for API
                    // convenience. They are evaluated in the timing-only context and do not
                    // participate in boolean condition gating.
                    let eval = match Program::compile(&condition.rule)
                        .map_err(|e| {
                            format!(
                                "Failed to compile condition '{}' (timing helper): {e}",
                                condition.description
                            )
                        })
                        .and_then(|p| {
                            p.execute(&timing_context).map_err(|e| {
                                format!(
                                    "Failed to execute condition '{}' (timing helper): {e}",
                                    condition.description
                                )
                            })
                        }) {
                        Ok(value) => {
                            let raw = Some(cel_value_to_json(&value));
                            let outcome = if parse_timing_value(&value).is_some() {
                                RuleEvalOutcome::Timing
                            } else {
                                RuleEvalOutcome::NonBool
                            };
                            RuleEvaluation {
                                index: idx as u32,
                                description: condition.description.clone(),
                                rule: condition.rule.clone(),
                                outcome,
                                bool_value: None,
                                raw_value: raw,
                                error: None,
                            }
                        }
                        Err(err) => RuleEvaluation {
                            index: idx as u32,
                            description: condition.description.clone(),
                            rule: condition.rule.clone(),
                            outcome: RuleEvalOutcome::Error,
                            bool_value: None,
                            raw_value: None,
                            error: Some(format!("{err}")),
                        },
                    };

                    conditions.push(eval);
                    continue;
                }

                let eval = match Program::compile(&condition.rule)
                    .map_err(|e| {
                        format!(
                            "Failed to compile condition '{}': {e}",
                            condition.description
                        )
                    })
                    .and_then(|p| {
                        p.execute(&condition_context).map_err(|e| {
                            format!(
                                "Failed to execute condition '{}': {e}",
                                condition.description
                            )
                        })
                    }) {
                    Ok(value) => match value {
                        cel::Value::Bool(b) => {
                            if !b {
                                conditions_passed = false;
                                stopped = true;
                            }
                            RuleEvaluation {
                                index: idx as u32,
                                description: condition.description.clone(),
                                rule: condition.rule.clone(),
                                outcome: if b {
                                    RuleEvalOutcome::True
                                } else {
                                    RuleEvalOutcome::False
                                },
                                bool_value: Some(b),
                                raw_value: Some(serde_json::Value::Bool(b)),
                                error: None,
                            }
                        }
                        other => {
                            conditions_passed = false;
                            stopped = true;
                            RuleEvaluation {
                                index: idx as u32,
                                description: condition.description.clone(),
                                rule: condition.rule.clone(),
                                outcome: RuleEvalOutcome::NonBool,
                                bool_value: None,
                                raw_value: Some(cel_value_to_json(&other)),
                                error: Some(format!(
                                    "Condition must return boolean, got: {:?}",
                                    other
                                )),
                            }
                        }
                    },
                    Err(err) => {
                        conditions_passed = false;
                        stopped = true;
                        RuleEvaluation {
                            index: idx as u32,
                            description: condition.description.clone(),
                            rule: condition.rule.clone(),
                            outcome: RuleEvalOutcome::Error,
                            bool_value: None,
                            raw_value: None,
                            error: Some(format!("{err}")),
                        }
                    }
                };

                conditions.push(eval);
            }
        }

        let overall_result = match precondition_policy {
            PreconditionPolicy::IgnoreGates => conditions_passed,
            PreconditionPolicy::EnforceAsGates => gate_passed && conditions_passed,
        };

        TriggerEvaluationReport {
            precondition_policy,
            precondition_gate_passed: gate_passed,
            conditions_passed,
            overall_result,
            preconditions,
            conditions,
        }
    }

    /// Extract timing configurations by evaluating CEL expressions.
    ///
    /// Sources:
    /// - `precondition`: cron/timer/event
    /// - `condition`: repeat frequency
    pub fn extract_timing(&self) -> Result<Vec<TriggerTiming>, String> {
        let context = create_precondition_context();
        let mut timings = Vec::new();

        // Timing sources:
        // - Precondition: cron/timer/event
        // - Condition: repeat frequency
        for rule in self.precondition.iter() {
            let expr = rule.rule.replace(' ', "");
            if !(expr.contains("cron(") || expr.contains("timer(") || expr.contains("event(")) {
                continue;
            }

            let program = Program::compile(&rule.rule).map_err(|e| {
                format!(
                    "Failed to compile timing rule '{}': {}",
                    rule.description, e
                )
            })?;

            let value = program.execute(&context).map_err(|e| {
                format!(
                    "Failed to execute timing rule '{}': {}",
                    rule.description, e
                )
            })?;

            if let Some(timing) = parse_timing_value(&value) {
                timings.push(timing);
            } else {
                return Err(format!(
                    "Timing rule '{}' did not return a valid timing value, got: {:?}",
                    rule.description, value
                ));
            }
        }

        for rule in self.condition.iter() {
            let expr = rule.rule.replace(' ', "");
            if !(expr.contains("repeat_per_day(") || expr.contains("repeat_per_week(")) {
                continue;
            }

            let program = Program::compile(&rule.rule).map_err(|e| {
                format!(
                    "Failed to compile timing rule '{}': {}",
                    rule.description, e
                )
            })?;

            let value = program.execute(&context).map_err(|e| {
                format!(
                    "Failed to execute timing rule '{}': {}",
                    rule.description, e
                )
            })?;

            if let Some(timing) = parse_timing_value(&value) {
                timings.push(timing);
            } else {
                return Err(format!(
                    "Timing rule '{}' did not return a valid timing value, got: {:?}",
                    rule.description, value
                ));
            }
        }

        Ok(timings)
    }

    /// Get timing rules as raw CEL expressions.
    ///
    /// This is a legacy helper used by downstream SDK code.
    pub fn get_timing_info(&self) -> Vec<String> {
        let mut out: Vec<String> = self.precondition.iter().map(|r| r.rule.clone()).collect();
        out.extend(
            self.condition
                .iter()
                .filter(|r| {
                    let expr = r.rule.replace(' ', "");
                    expr.contains("repeat_per_day(") || expr.contains("repeat_per_week(")
                })
                .map(|r| r.rule.clone()),
        );
        out
    }
}

fn is_timing_precondition(expr: &str) -> bool {
    // Keep this simple and explicit: timing extraction only supports these helper fns.
    // Anything else is treated as a gate or non-timing precondition.
    let expr = expr.replace(' ', "");
    expr.contains("cron(")
        || expr.contains("timer(")
        || expr.contains("event(")
        || expr.contains("repeat_per_day(")
        || expr.contains("repeat_per_week(")
}

pub fn resolve_timer_literal(
    timer_value: &str,
    anchor: DateTime<Utc>,
    timezone_offset: &str,
) -> Result<DateTime<Utc>, String> {
    if let Ok(at) = DateTime::parse_from_rfc3339(timer_value) {
        return Ok(at.with_timezone(&Utc));
    }

    if let Ok(local) = parse_local_datetime_without_offset(timer_value) {
        let offset = parse_offset(timezone_offset);
        return offset
            .from_local_datetime(&local)
            .single()
            .map(|dt| dt.with_timezone(&Utc))
            .ok_or_else(|| format!("Failed to resolve local timer literal '{timer_value}' with timezone {timezone_offset}"));
    }

    let duration = parse_duration_literal(timer_value)?;
    anchor.checked_add_signed(duration).ok_or_else(|| {
        format!("Timer literal '{timer_value}' overflowed when applied to anchor {anchor}")
    })
}

pub fn normalize_timer_literal(
    timer_value: &str,
    anchor: DateTime<Utc>,
    timezone_offset: &str,
) -> Result<String, String> {
    resolve_timer_literal(timer_value, anchor, timezone_offset).map(|dt| dt.to_rfc3339())
}

fn cel_value_to_json(value: &cel::Value) -> serde_json::Value {
    match value {
        cel::Value::Null => serde_json::Value::Null,
        cel::Value::Bool(b) => serde_json::Value::Bool(*b),
        cel::Value::Int(i) => serde_json::Value::Number((*i).into()),
        cel::Value::UInt(u) => serde_json::Value::Number(serde_json::Number::from(*u)),
        cel::Value::String(s) => serde_json::Value::String(s.to_string()),
        other => serde_json::Value::String(format!("{other:?}")),
    }
}

fn current_event_to_json(event: Option<&MonitoringEvent>) -> serde_json::Value {
    let Some(event) = event else {
        return serde_json::Value::Object(serde_json::Map::new());
    };

    let metadata = serde_json::from_str::<serde_json::Value>(&event.metadata_json)
        .unwrap_or_else(|_| serde_json::json!({ "raw": event.metadata_json }));

    let mut payload = serde_json::Map::new();
    payload.insert(
        "event_type".to_string(),
        serde_json::Value::String(event.event_type.clone()),
    );
    payload.insert(
        "timestamp".to_string(),
        serde_json::Value::String(event.timestamp.clone()),
    );
    payload.insert("metadata".to_string(), metadata.clone());

    if let serde_json::Value::Object(metadata_object) = metadata {
        for (key, value) in metadata_object {
            payload.entry(key).or_insert(value);
        }
    }

    serde_json::Value::Object(payload)
}

/// Create a CEL context with all helper functions
fn create_cel_context<'a>(eval_ctx: &'a EvaluationContext<'a>) -> Context<'a> {
    let mut context = Context::default();

    // Clone events for use in closures
    let events = eval_ctx.events.to_vec();
    let current_time = eval_ctx.current_time;
    let timezone_offset = eval_ctx.timezone_offset.to_string();
    let current_event = current_event_to_json(eval_ctx.current_event);

    let _ = context.add_variable("event", current_event.clone());
    let _ = context.add_variable("current_event", current_event);

    // Add event_count function: event_count(minutes, event_type) -> int
    {
        let events_clone = events.clone();
        let tz_clone = timezone_offset.clone();
        context.add_function(
            "event_count",
            move |minutes: i64, event_type: Arc<String>| {
                count_events(&events_clone, minutes, &event_type, current_time, &tz_clone)
            },
        );
    }

    // Add list_event function (alias for event_count for compatibility)
    {
        let events_clone = events.clone();
        let tz_clone = timezone_offset.clone();
        context.add_function("list_event", move |event_type: Arc<String>| {
            count_events(&events_clone, 60, &event_type, current_time, &tz_clone)
        });
    }

    // Add event_exists function: event_exists(minutes, event_type) -> bool
    {
        let events_clone = events.clone();
        let tz_clone = timezone_offset.clone();
        context.add_function(
            "event_exists",
            move |minutes: i64, event_type: Arc<String>| {
                count_events(&events_clone, minutes, &event_type, current_time, &tz_clone) > 0
            },
        );
    }

    // Add event_exists_with_message function: event_exists_with_message(minutes, event_type, message_substring) -> bool
    {
        let events_clone = events.clone();
        let tz_clone = timezone_offset.clone();
        context.add_function(
            "event_exists_with_message",
            move |minutes: i64, event_type: Arc<String>, msg_substr: Arc<String>| {
                count_events_with_message(
                    &events_clone,
                    minutes,
                    &event_type,
                    &msg_substr,
                    current_time,
                    &tz_clone,
                ) > 0
            },
        );
    }

    // Add cron function for preconditions using POSIX cron format (5 fields):
    // cron("minute hour day-of-month month day-of-week") -> string
    // Example: cron("0 9 * * *") for daily at 9 AM
    context.add_function("cron", |expr: Arc<String>| expr.to_string());

    // Add in_time_range function: in_time_range("09:00", "17:00") -> bool
    {
        let tz_clone = timezone_offset.clone();
        context.add_function(
            "in_time_range",
            move |start: Arc<String>, end: Arc<String>| {
                is_in_time_range(&start, &end, current_time, &tz_clone)
            },
        );
    }

    // Add is_weekday function: is_weekday(["mon", "tue", "wed", "thu", "fri"]) -> bool
    {
        let tz_clone = timezone_offset.clone();
        context.add_function("is_weekday", move |days: Arc<Vec<cel::Value>>| {
            let day_strings: Vec<String> = days
                .iter()
                .filter_map(|v| {
                    if let cel::Value::String(s) = v {
                        Some(s.to_string())
                    } else {
                        None
                    }
                })
                .collect();
            is_weekday(&day_strings, current_time, &tz_clone)
        });
    }

    // Add current_hour function: current_hour() -> int
    {
        let tz_clone = timezone_offset.clone();
        context.add_function("current_hour", move || {
            get_current_hour(current_time, &tz_clone)
        });
    }

    context
}

/// Create a CEL context for precondition evaluation
/// These functions return JSON-encoded timing information strings
fn create_precondition_context<'a>() -> Context<'a> {
    let mut context = Context::default();

    // cron(expression) -> JSON string: {"type":"cron","expression":"..."}
    context.add_function("cron", |expr: Arc<String>| {
        format!(r#"{{"type":"cron","expression":"{}"}}"#, expr)
    });

    // timer(value) -> JSON string: {"type":"timer","value":"..."}
    context.add_function("timer", |value: Arc<String>| {
        format!(r#"{{"type":"timer","value":"{}"}}"#, value)
    });

    // event(name) -> JSON string: {"type":"event","event_type":"..."}
    context.add_function("event", |event_type: Arc<String>| {
        format!(r#"{{"type":"event","event_type":"{}"}}"#, event_type)
    });

    // repeat_per_day(n) -> JSON string: {"type":"repeat_frequency","per_day":n}
    context.add_function("repeat_per_day", |n: i64| {
        format!(r#"{{"type":"repeat_frequency","per_day":{}}}"#, n)
    });

    // repeat_per_week(n) -> JSON string: {"type":"repeat_frequency","per_week":n}
    context.add_function("repeat_per_week", |n: i64| {
        format!(r#"{{"type":"repeat_frequency","per_week":{}}}"#, n)
    });

    // ──────────────────────────────────────────────────────────────────────────
    // Location helper functions (return JSON-encoded location objects)
    // These are used within condition expressions to create Location values
    // ──────────────────────────────────────────────────────────────────────────

    // current_location() -> JSON string representing a location request
    // Returns: {"type":"current_location"}
    // Used in conditions to get the current GPS position at evaluation time
    context.add_function("current_location", || {
        r#"{"type":"current_location"}"#.to_string()
    });

    // location_at(lat, lng) -> JSON string representing a coordinate
    // Returns: {"type":"coordinate","lat":..,"lng":..,"coord_system":"wgs84"}
    context.add_function("location_at", |lat: f64, lng: f64| {
        format!(
            r#"{{"type":"coordinate","lat":{},"lng":{},"coord_system":"wgs84"}}"#,
            lat, lng
        )
    });

    // location_name(name) -> JSON string representing a named location
    // Returns: {"type":"named","name":"..."}
    // The actual geocoding happens at evaluation time via the location service
    context.add_function("location_name", |name: Arc<String>| {
        format!(r#"{{"type":"named","name":"{}"}}"#, name)
    });

    // is_location_in_range(location_json, target_or_name, radius_meters) -> bool placeholder
    // This returns a JSON descriptor for the SDK to evaluate
    // Returns: {"type":"range_check","location":...,"target":"...","radius":...}
    context.add_function(
        "is_location_in_range",
        |location: Arc<String>, target: Arc<String>, radius: f64| {
            format!(
                r#"{{"type":"range_check","location":{},"target":"{}","radius":{}}}"#,
                location, target, radius
            )
        },
    );

    // is_location_close(location_json, target_or_name, radius_meters) -> bool placeholder
    // Alias for is_location_in_range with a more intuitive name
    context.add_function(
        "is_location_close",
        |location: Arc<String>, target: Arc<String>, radius: f64| {
            format!(
                r#"{{"type":"range_check","location":{},"target":"{}","radius":{}}}"#,
                location, target, radius
            )
        },
    );

    context
}

/// Parse a CEL value returned from precondition functions into a TriggerTiming enum
fn parse_timing_value(value: &cel::Value) -> Option<TriggerTiming> {
    match value {
        cel::Value::String(s) => {
            // Parse JSON-encoded timing value
            let s = s.to_string();

            // Try to parse as JSON
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&s) {
                if let Some(timing_type) = json.get("type").and_then(|v| v.as_str()) {
                    return match timing_type {
                        "cron" => json.get("expression").and_then(|v| v.as_str()).map(|expr| {
                            TriggerTiming::Cron {
                                expression: expr.to_string(),
                            }
                        }),
                        "timer" => json.get("value").and_then(|v| v.as_str()).map(|value| {
                            TriggerTiming::Timer {
                                value: value.to_string(),
                            }
                        }),
                        "event" => {
                            json.get("event_type")
                                .and_then(|v| v.as_str())
                                .map(|event_type| TriggerTiming::Event {
                                    event_type: event_type.to_string(),
                                })
                        }
                        "repeat_frequency" => {
                            if let Some(n) = json.get("per_day").and_then(|v| v.as_i64()) {
                                Some(TriggerTiming::RepeatFrequency {
                                    frequency: RepeatFrequency::PerDay(n as u32),
                                })
                            } else if let Some(n) = json.get("per_week").and_then(|v| v.as_i64()) {
                                Some(TriggerTiming::RepeatFrequency {
                                    frequency: RepeatFrequency::PerWeek(n as u32),
                                })
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                }
            }

            // Legacy: If it looks like a plain cron expression, treat it as such
            if s.split_whitespace().count() == 5
                && s.chars().all(|c| {
                    c.is_ascii_digit() || c == '*' || c == '/' || c == '-' || c == ',' || c == ' '
                })
            {
                Some(TriggerTiming::Cron { expression: s })
            } else {
                None
            }
        }
        _ => None,
    }
}

// Helper functions

fn count_events(
    events: &[MonitoringEvent],
    past_minutes: i64,
    event_type: &str,
    current_time: i64,
    tz_offset: &str,
) -> i64 {
    let cutoff_time = current_time - (past_minutes * 60);

    events
        .iter()
        .filter(|event| {
            let event_time = parse_event_timestamp(&event.timestamp, tz_offset);
            event_time >= cutoff_time && (event_type.is_empty() || &event.event_type == event_type)
        })
        .count() as i64
}

fn count_events_with_message(
    events: &[MonitoringEvent],
    past_minutes: i64,
    event_type: &str,
    message_substr: &str,
    current_time: i64,
    tz_offset: &str,
) -> i64 {
    let cutoff_time = current_time - (past_minutes * 60);

    events
        .iter()
        .filter(|event| {
            let event_time = parse_event_timestamp(&event.timestamp, tz_offset);
            if event_time < cutoff_time {
                return false;
            }
            if !event_type.is_empty() && event.event_type != event_type {
                return false;
            }
            // Extract message from metadata_json
            if let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&event.metadata_json) {
                if let Some(message) = metadata.get("message").and_then(|v| v.as_str()) {
                    return message.contains(message_substr);
                }
            }
            false
        })
        .count() as i64
}

fn is_in_time_range(start: &str, end: &str, current_time: i64, tz_offset: &str) -> bool {
    let offset = parse_offset(tz_offset);
    let local_time = match Utc.timestamp_opt(current_time, 0).single() {
        Some(utc) => utc.with_timezone(&offset),
        None => return false,
    };

    let current_minutes = local_time.hour() * 60 + local_time.minute();
    let start_minutes = parse_time_to_minutes(start);
    let end_minutes = parse_time_to_minutes(end);

    if start_minutes <= end_minutes {
        current_minutes >= start_minutes && current_minutes <= end_minutes
    } else {
        current_minutes >= start_minutes || current_minutes <= end_minutes
    }
}

fn is_weekday(weekdays: &[String], current_time: i64, tz_offset: &str) -> bool {
    let offset = parse_offset(tz_offset);
    let local_time = match Utc.timestamp_opt(current_time, 0).single() {
        Some(utc) => utc.with_timezone(&offset),
        None => return false,
    };

    let current_weekday = match local_time.weekday() {
        chrono::Weekday::Mon => "mon",
        chrono::Weekday::Tue => "tue",
        chrono::Weekday::Wed => "wed",
        chrono::Weekday::Thu => "thu",
        chrono::Weekday::Fri => "fri",
        chrono::Weekday::Sat => "sat",
        chrono::Weekday::Sun => "sun",
    };

    weekdays.iter().any(|wd| wd.as_str() == current_weekday)
}

fn get_current_hour(current_time: i64, tz_offset: &str) -> i64 {
    let offset = parse_offset(tz_offset);
    match Utc.timestamp_opt(current_time, 0).single() {
        Some(utc) => {
            let local_time = utc.with_timezone(&offset);
            local_time.hour() as i64
        }
        None => 0,
    }
}

fn parse_event_timestamp(timestamp: &str, _tz_offset: &str) -> i64 {
    // Try RFC3339 first
    if let Ok(event_time) = DateTime::parse_from_rfc3339(timestamp) {
        return event_time.timestamp();
    }

    // Try Unix timestamp
    if let Ok(unix_seconds) = timestamp.parse::<i64>() {
        return unix_seconds;
    }

    // Default to 0 if parsing fails
    0
}

fn parse_offset(tz_offset: &str) -> FixedOffset {
    FixedOffset::from_str(tz_offset).unwrap_or_else(|_| {
        FixedOffset::east_opt(8 * 3600).expect("UTC+08:00 offset should be valid")
    })
}

fn parse_local_datetime_without_offset(value: &str) -> Result<NaiveDateTime, String> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f"))
        .map_err(|_| format!("Unsupported timer literal '{value}'"))
}

fn parse_duration_literal(value: &str) -> Result<chrono::Duration, String> {
    let mut remaining = value.trim();
    if remaining.is_empty() {
        return Err("Timer literal must not be empty".to_string());
    }

    let mut total_millis: i64 = 0;
    while !remaining.is_empty() {
        let digits_end = remaining
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("Timer literal '{value}' is missing a unit after number"))?;
        if digits_end == 0 {
            return Err(format!(
                "Timer literal '{value}' must start each segment with digits"
            ));
        }

        let amount: i64 = remaining[..digits_end]
            .parse()
            .map_err(|_| format!("Invalid number in timer literal '{value}'"))?;
        let unit_rest = &remaining[digits_end..];
        let unit_end = unit_rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(unit_rest.len());
        let unit = unit_rest[..unit_end].trim().to_ascii_lowercase();
        let millis_per_unit = match unit.as_str() {
            "ms" | "msec" | "millisecond" | "milliseconds" => 1,
            "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000,
            "d" | "day" | "days" => 86_400_000,
            _ => {
                return Err(format!(
                    "Unsupported timer unit '{unit}' in literal '{value}'"
                ))
            }
        };
        total_millis = total_millis
            .checked_add(amount.checked_mul(millis_per_unit).ok_or_else(|| {
                format!("Timer literal '{value}' overflowed while parsing duration")
            })?)
            .ok_or_else(|| format!("Timer literal '{value}' overflowed while summing duration"))?;

        remaining = unit_rest[unit_end..].trim_start();
    }

    Ok(chrono::Duration::milliseconds(total_millis))
}

fn parse_time_to_minutes(time_str: &str) -> u32 {
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() != 2 {
        return 0;
    }

    let hours: u32 = parts[0].parse().unwrap_or(0);
    let minutes: u32 = parts[1].parse().unwrap_or(0);

    hours * 60 + minutes
}

#[cfg(test)]
mod realworld_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn create_event(event_type: &str, message: &str, timestamp: i64) -> MonitoringEvent {
        // Message is now stored in metadata_json
        let metadata = serde_json::json!({"message": message});
        MonitoringEvent {
            event_type: event_type.to_string(),
            timestamp: timestamp.to_string(),
            metadata_json: metadata.to_string(),
        }
    }

    #[test]
    fn test_cel_event_count() {
        let events = vec![
            create_event("error", "Test error 1", 1950),
            create_event("error", "Test error 2", 1960),
            create_event("warning", "Test warning", 1970),
        ];

        let ctx = EvaluationContext {
            events: &events,
            current_event: None,
            current_time: 2000,
            timezone_offset: "+08:00",
        };

        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: "event_count(1, 'error') >= 2".to_string(),
                description: "At least 2 errors".to_string(),
            }],
        };

        let result = config.evaluate(&ctx);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_cel_event_exists() {
        let events = vec![create_event("error", "Test error", 1950)];

        let ctx = EvaluationContext {
            events: &events,
            current_event: None,
            current_time: 2000,
            timezone_offset: "+08:00",
        };

        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: "event_exists(1, 'error')".to_string(),
                description: "Error exists".to_string(),
            }],
        };

        let result = config.evaluate(&ctx);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_cel_complex_expression() {
        let events = vec![
            create_event("error", "database error", 1950),
            create_event("warning", "test warning", 1960),
        ];

        let ctx = EvaluationContext {
            events: &events,
            current_event: None,
            current_time: 2000,
            timezone_offset: "+08:00",
        };

        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: "(event_count(1, 'error') > 0 || event_count(1, 'warning') > 2) && event_count(1, '') >= 2".to_string(),
                description: "Complex logic".to_string(),
            }],
        };

        let result = config.evaluate(&ctx);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_cel_time_range() {
        // Test at 10:00 local time for +08:00 (02:00 UTC)
        let ctx = EvaluationContext {
            events: &[],
            current_event: None,
            current_time: 7200,
            timezone_offset: "+08:00",
        };

        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: "in_time_range('09:00', '17:00')".to_string(),
                description: "Business hours".to_string(),
            }],
        };

        let result = config.evaluate(&ctx);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_cel_with_message() {
        let events = vec![
            create_event("error", "database connection failed", 1950),
            create_event("error", "test error", 1960),
        ];

        let ctx = EvaluationContext {
            events: &events,
            current_event: None,
            current_time: 2000,
            timezone_offset: "+08:00",
        };

        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: "event_exists_with_message(1, 'error', 'database')".to_string(),
                description: "Database error".to_string(),
            }],
        };

        let result = config.evaluate(&ctx);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_multiple_conditions() {
        let events = vec![
            create_event("error", "test", 1950),
            create_event("warning", "test", 1960),
        ];

        let ctx = EvaluationContext {
            events: &events,
            current_event: None,
            current_time: 2000,
            timezone_offset: "+08:00",
        };

        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![
                Rule {
                    rule: "event_count(1, 'error') > 0".to_string(),
                    description: "Has errors".to_string(),
                },
                Rule {
                    rule: "event_count(1, '') >= 2".to_string(),
                    description: "At least 2 events total".to_string(),
                },
            ],
        };

        let result = config.evaluate(&ctx);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_parse_json_config() {
        let json = r#"{
            "name": "Test Trigger",
            "precondition": [
                {
                    "rule": "cron('0 18 * * *')",
                    "description": "Every day at 6 PM"
                }
            ],
            "condition": [
                {
                    "rule": "list_event('') == 0",
                    "description": "idle"
                }
            ]
        }"#;

        let config = TriggerConfig::from_json(json);
        assert!(config.is_ok());
    }

    #[test]
    fn test_extract_timing_cron() {
        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![Rule {
                rule: "cron('0 18 * * *')".to_string(),
                description: "Every day at 6 PM".to_string(),
            }],
            condition: vec![],
        };

        let timings = config.extract_timing().expect("should parse timing");
        assert_eq!(timings.len(), 1);
        match &timings[0] {
            TriggerTiming::Cron { expression } => {
                assert_eq!(expression, "0 18 * * *");
            }
            _ => panic!("Expected Cron timing"),
        }
    }

    #[test]
    fn test_extract_timing_timer() {
        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![Rule {
                rule: "timer('1min1s')".to_string(),
                description: "One minute and one second later".to_string(),
            }],
            condition: vec![],
        };

        let timings = config.extract_timing().expect("should parse timing");
        assert_eq!(timings.len(), 1);
        assert!(matches!(
            &timings[0],
            TriggerTiming::Timer { value } if value == "1min1s"
        ));
    }

    #[test]
    fn test_extract_timing_event() {
        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![Rule {
                rule: "event('Connectivity')".to_string(),
                description: "Connectivity event".to_string(),
            }],
            condition: vec![],
        };

        let timings = config.extract_timing().expect("should parse timing");
        assert_eq!(timings.len(), 1);
        assert!(matches!(
            &timings[0],
            TriggerTiming::Event { event_type } if event_type == "Connectivity"
        ));
    }

    #[test]
    fn test_extract_timing_repeat_per_day() {
        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: "repeat_per_day(3)".to_string(),
                description: "3 times per day".to_string(),
            }],
        };

        let timings = config.extract_timing().expect("should parse timing");
        assert_eq!(timings.len(), 1);
        match &timings[0] {
            TriggerTiming::RepeatFrequency {
                frequency: RepeatFrequency::PerDay(n),
            } => {
                assert_eq!(*n, 3);
            }
            _ => panic!("Expected RepeatFrequency::PerDay"),
        }
    }

    #[test]
    fn test_extract_timing_repeat_per_week() {
        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: "repeat_per_week(2)".to_string(),
                description: "Twice per week".to_string(),
            }],
        };

        let timings = config.extract_timing().expect("should parse timing");
        assert_eq!(timings.len(), 1);
        match &timings[0] {
            TriggerTiming::RepeatFrequency {
                frequency: RepeatFrequency::PerWeek(n),
            } => {
                assert_eq!(*n, 2);
            }
            _ => panic!("Expected RepeatFrequency::PerWeek"),
        }
    }

    #[test]
    fn test_extract_timing_multiple_preconditions() {
        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![
                Rule {
                    rule: "cron('0 9 * * 1-5')".to_string(),
                    description: "Weekdays at 9 AM".to_string(),
                },
                Rule {
                    rule: "event('Location')".to_string(),
                    description: "Location event".to_string(),
                },
            ],
            condition: vec![],
        };

        let timings = config.extract_timing().expect("should parse timing");
        assert_eq!(timings.len(), 2);
        assert!(
            matches!(&timings[0], TriggerTiming::Cron { expression } if expression == "0 9 * * 1-5")
        );
        assert!(matches!(
            &timings[1],
            TriggerTiming::Event { event_type } if event_type == "Location"
        ));
    }

    #[test]
    fn test_resolve_timer_literal_duration() {
        let anchor = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).single().unwrap();
        let resolved = resolve_timer_literal("1min1s", anchor, "+08:00").expect("resolve timer");
        assert_eq!(resolved.timestamp(), anchor.timestamp() + 61);
    }

    #[test]
    fn test_resolve_timer_literal_local_datetime() {
        let anchor = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).single().unwrap();
        let resolved = resolve_timer_literal("2026-04-01T09:30:00", anchor, "+08:00")
            .expect("resolve timer local datetime");
        assert_eq!(resolved.to_rfc3339(), "2026-04-01T01:30:00+00:00");
    }

    #[test]
    fn test_extract_timing_ignores_gate_preconditions() {
        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![
                Rule {
                    rule: "cron('0 9 * * *')".to_string(),
                    description: "Every day at 9 AM".to_string(),
                },
                Rule {
                    rule: "current_hour() >= 9".to_string(),
                    description: "After 9".to_string(),
                },
            ],
            condition: vec![],
        };

        let timings = config.extract_timing().expect("should parse timing");
        assert_eq!(timings.len(), 1);
        assert!(
            matches!(&timings[0], TriggerTiming::Cron { expression } if expression == "0 9 * * *")
        );
    }

    #[test]
    fn test_evaluate_detailed_gate_blocks_conditions_when_enforced() {
        let ctx = EvaluationContext {
            events: &[],
            current_event: None,
            current_time: 36000,
            timezone_offset: "+08:00",
        };

        let config = TriggerConfig {
            name: "Test".to_string(),
            version: "v1".to_string(),
            precondition: vec![Rule {
                rule: "current_hour() < 0".to_string(),
                description: "Impossible gate".to_string(),
            }],
            condition: vec![Rule {
                rule: "true".to_string(),
                description: "Would be true".to_string(),
            }],
        };

        let report = config.evaluate_detailed(&ctx, PreconditionPolicy::EnforceAsGates);
        assert!(!report.precondition_gate_passed);
        assert!(!report.overall_result);
        assert_eq!(report.conditions.len(), 1);
        assert!(matches!(
            report.conditions[0].outcome,
            RuleEvalOutcome::Skipped
        ));
    }

    #[test]
    fn test_current_event_metadata_is_available_in_conditions() {
        let events = vec![MonitoringEvent {
            event_type: "app_foreground".to_string(),
            timestamp: "2026-04-02T00:39:13+00:00".to_string(),
            metadata_json: serde_json::json!({
                "app": "VSCode",
                "window": "editor"
            })
            .to_string(),
        }];

        let ctx = EvaluationContext {
            events: &events,
            current_event: events.first(),
            current_time: 1_743_557_953,
            timezone_offset: "+00:00",
        };

        let config = TriggerConfig {
            name: "Current event metadata".to_string(),
            version: "v1".to_string(),
            precondition: vec![],
            condition: vec![Rule {
                rule: "event.app == 'VSCode' && event.event_type == 'app_foreground'".to_string(),
                description: "Current event metadata is exposed".to_string(),
            }],
        };

        let result = config.evaluate(&ctx);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }
}
