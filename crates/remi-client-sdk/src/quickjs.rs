use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use rquickjs_core::function::Func;
use rquickjs_core::{Context, Ctx, Error as QuickJsError, Object, Runtime};
use serde_json::{Value as JsonValue, json};

use crate::external_tool_handler::ExternalToolHandler;
use crate::external_tools::ExternalToolExecutor;

pub type QuickJsHostHandler = Arc<dyn Fn(JsonValue) -> Result<JsonValue, String> + Send + Sync>;

#[derive(Clone, Default)]
pub struct QuickJsHostBindings {
    pub http_request: Option<QuickJsHostHandler>,
    pub notify_send: Option<QuickJsHostHandler>,
    pub notify_list: Option<QuickJsHostHandler>,
    pub notify_mark_read: Option<QuickJsHostHandler>,
    pub notify_mark_category_read: Option<QuickJsHostHandler>,
    pub notify_mark_all_read: Option<QuickJsHostHandler>,
    pub notify_delete_category: Option<QuickJsHostHandler>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickJsSmokeOutput {
    pub json_result: String,
    pub console_logs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickJsSmokeError {
    message: String,
    exception: Option<String>,
}

impl QuickJsSmokeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exception: None,
        }
    }

    fn with_exception(message: impl Into<String>, exception: Option<String>) -> Self {
        Self {
            message: message.into(),
            exception,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn exception(&self) -> Option<&str> {
        self.exception.as_deref()
    }
}

impl fmt::Display for QuickJsSmokeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.exception() {
            Some(exception) if !exception.trim().is_empty() => {
                write!(f, "{}: {}", self.message, exception)
            }
            _ => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for QuickJsSmokeError {}

pub struct QuickJsSmokeHandler;

#[async_trait]
impl ExternalToolHandler for QuickJsSmokeHandler {
    async fn handle(&self, _tool_call_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let script = extract_script(payload)?;
        let output = quickjs_smoke_eval(script).map_err(|error| error.to_string())?;
        Ok(json!({
            "json_result": output.json_result,
            "console_logs": output.console_logs,
        }))
    }
}

pub fn register_quickjs_external_tools(executor: &mut ExternalToolExecutor) {
    executor.register("quickjs_eval_request", QuickJsSmokeHandler);
}

pub fn quickjs_smoke_eval(script: &str) -> Result<QuickJsSmokeOutput, QuickJsSmokeError> {
    quickjs_eval_with_bindings(script, QuickJsHostBindings::default())
}

pub fn quickjs_eval_with_bindings(
    script: &str,
    bindings: QuickJsHostBindings,
) -> Result<QuickJsSmokeOutput, QuickJsSmokeError> {
    let started_at = Instant::now();
    tracing::info!(
        script_len = script.len(),
        "[QuickJs] Creating smoke runtime"
    );

    let runtime = Runtime::new().map_err(|error| {
        QuickJsSmokeError::new(format!("failed to create QuickJS runtime: {error}"))
    })?;
    runtime.set_memory_limit(8 * 1024 * 1024);
    runtime.set_max_stack_size(512 * 1024);

    let context = Context::full(&runtime).map_err(|error| {
        QuickJsSmokeError::new(format!("failed to create QuickJS context: {error}"))
    })?;
    let captured_logs = Arc::new(Mutex::new(Vec::new()));

    tracing::info!(
        script_len = script.len(),
        "[QuickJs] Starting smoke evaluation"
    );
    let result =
        context.with(|ctx| run_eval(ctx, script, Arc::clone(&captured_logs), bindings.clone()));

    match &result {
        Ok(output) => tracing::info!(
            elapsed_ms = started_at.elapsed().as_millis(),
            console_log_count = output.console_logs.len(),
            json_result = %output.json_result,
            "[QuickJs] Smoke evaluation finished"
        ),
        Err(error) => tracing::warn!(
            elapsed_ms = started_at.elapsed().as_millis(),
            error = %error,
            "[QuickJs] Smoke evaluation failed"
        ),
    }

    result
}

fn run_eval(
    ctx: Ctx<'_>,
    script: &str,
    captured_logs: Arc<Mutex<Vec<String>>>,
    bindings: QuickJsHostBindings,
) -> Result<QuickJsSmokeOutput, QuickJsSmokeError> {
    install_console(ctx.clone(), Arc::clone(&captured_logs))?;
    install_host_bindings(ctx.clone(), bindings)?;

    let wrapped_script = wrap_smoke_script(script);
    let json_result = ctx.eval::<String, _>(wrapped_script).map_err(|error| {
        QuickJsSmokeError::with_exception(
            format!("QuickJS script evaluation failed: {error}"),
            capture_exception(ctx.clone(), &error),
        )
    })?;

    while ctx.execute_pending_job() {}

    let console_logs = captured_logs
        .lock()
        .map(|logs| logs.clone())
        .unwrap_or_default();

    Ok(QuickJsSmokeOutput {
        json_result,
        console_logs,
    })
}

fn install_console(
    ctx: Ctx<'_>,
    captured_logs: Arc<Mutex<Vec<String>>>,
) -> Result<(), QuickJsSmokeError> {
    let globals = ctx.globals();
    let console = Object::new(ctx.clone()).map_err(|error| {
        QuickJsSmokeError::new(format!("failed to create QuickJS console object: {error}"))
    })?;

    let log_handler = move |message: String| {
        tracing::info!(message = %message, "[QuickJs] console.log");
        if let Ok(mut logs) = captured_logs.lock() {
            logs.push(message);
        }
    };

    console
        .set("log", Func::new(log_handler))
        .map_err(|error| QuickJsSmokeError::new(format!("failed to bind console.log: {error}")))?;
    globals.set("console", console).map_err(|error| {
        QuickJsSmokeError::new(format!("failed to install console object: {error}"))
    })?;
    Ok(())
}

fn install_host_bindings(
    ctx: Ctx<'_>,
    bindings: QuickJsHostBindings,
) -> Result<(), QuickJsSmokeError> {
    install_json_host_function(ctx.clone(), "__remi_http_request", bindings.http_request)?;
    install_json_host_function(ctx.clone(), "__remi_notify_send", bindings.notify_send)?;
    install_json_host_function(ctx.clone(), "__remi_notify_list", bindings.notify_list)?;
    install_json_host_function(
        ctx.clone(),
        "__remi_notify_mark_read",
        bindings.notify_mark_read,
    )?;
    install_json_host_function(
        ctx.clone(),
        "__remi_notify_mark_category_read",
        bindings.notify_mark_category_read,
    )?;
    install_json_host_function(
        ctx.clone(),
        "__remi_notify_mark_all_read",
        bindings.notify_mark_all_read,
    )?;
    install_json_host_function(
        ctx.clone(),
        "__remi_notify_delete_category",
        bindings.notify_delete_category,
    )?;

    ctx.eval::<(), _>(host_shim_script()).map_err(|error| {
        QuickJsSmokeError::with_exception(
            format!("failed to install QuickJS host shims: {error}"),
            capture_exception(ctx.clone(), &error),
        )
    })?;

    Ok(())
}

fn install_json_host_function(
    ctx: Ctx<'_>,
    name: &str,
    handler: Option<QuickJsHostHandler>,
) -> Result<(), QuickJsSmokeError> {
    let Some(handler) = handler else {
        return Ok(());
    };

    let globals = ctx.globals();
    let function = Func::new(move |payload_json: String| -> String {
        let payload = serde_json::from_str::<JsonValue>(&payload_json)
            .map_err(|error| format!("invalid host payload JSON: {error}"))
            .and_then(|payload| handler(payload));

        match payload {
            Ok(value) => json!({ "ok": true, "value": value }).to_string(),
            Err(error) => json!({ "ok": false, "error": error }).to_string(),
        }
    });

    globals.set(name, function).map_err(|error| {
        QuickJsSmokeError::new(format!(
            "failed to install QuickJS host function '{name}': {error}"
        ))
    })
}

fn host_shim_script() -> &'static str {
    r#"
const __remiDecodeHostResult = (raw, label) => {
    let parsed;
    try {
        parsed = JSON.parse(String(raw ?? '{"ok":false,"error":"empty host result"}'));
    } catch (error) {
        throw new Error(`${label} returned invalid JSON: ${error?.message ?? error}`);
    }

    if (!parsed || parsed.ok !== true) {
        throw new Error(`${label} failed: ${parsed?.error ?? 'unknown error'}`);
    }

    return parsed.value;
};

if (typeof __remi_http_request === 'function') {
    globalThis.http = {
        request(request) {
            return __remiDecodeHostResult(
                __remi_http_request(JSON.stringify(request ?? {})),
                'http.request'
            );
        },
        get(url, options = {}) {
            return this.request({ ...options, method: 'GET', url });
        },
        post(url, options = {}) {
            return this.request({ ...options, method: 'POST', url });
        },
        put(url, options = {}) {
            return this.request({ ...options, method: 'PUT', url });
        },
        patch(url, options = {}) {
            return this.request({ ...options, method: 'PATCH', url });
        },
        delete(url, options = {}) {
            return this.request({ ...options, method: 'DELETE', url });
        },
        head(url, options = {}) {
            return this.request({ ...options, method: 'HEAD', url });
        },
    };
}

if (typeof __remi_notify_send === 'function') {
    globalThis.notify = {
        send(request) {
            return __remiDecodeHostResult(
                __remi_notify_send(JSON.stringify(request ?? {})),
                'notify.send'
            );
        },
        list(request = {}) {
            return __remiDecodeHostResult(
                __remi_notify_list(JSON.stringify(request ?? {})),
                'notify.list'
            );
        },
        markRead(notificationId) {
            return __remiDecodeHostResult(
                __remi_notify_mark_read(JSON.stringify({ notificationId })),
                'notify.markRead'
            );
        },
        markCategoryRead(category) {
            return __remiDecodeHostResult(
                __remi_notify_mark_category_read(JSON.stringify({ category })),
                'notify.markCategoryRead'
            );
        },
        markAllRead() {
            return __remiDecodeHostResult(
                __remi_notify_mark_all_read(JSON.stringify({})),
                'notify.markAllRead'
            );
        },
        deleteCategory(category) {
            return __remiDecodeHostResult(
                __remi_notify_delete_category(JSON.stringify({ category })),
                'notify.deleteCategory'
            );
        },
    };
}
"#
}

fn wrap_smoke_script(script: &str) -> String {
    format!(
        "const __remi_result = (() => {{\n{script}\n}})();\nJSON.stringify(__remi_result ?? null);"
    )
}

fn capture_exception(ctx: Ctx<'_>, error: &QuickJsError) -> Option<String> {
    match error {
        QuickJsError::Exception => {
            let exception = ctx.catch();
            ctx.json_stringify(exception)
                .ok()
                .and_then(|value| value.and_then(|text| text.to_string().ok()))
                .or_else(|| Some("<unprintable JavaScript exception>".to_string()))
        }
        _ => None,
    }
}

fn extract_script(payload: &JsonValue) -> Result<&str, String> {
    payload
        .get("script")
        .and_then(JsonValue::as_str)
        .or_else(|| {
            payload
                .get("arguments")
                .and_then(|value| value.get("script"))
                .and_then(JsonValue::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "quickjs_eval_request requires a non-empty script".to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        QuickJsHostBindings, quickjs_eval_with_bindings, quickjs_smoke_eval,
        register_quickjs_external_tools,
    };
    use crate::external_tools::{ExternalToolCallRequest, ExternalToolExecutor};
    use serde_json::json;

    #[test]
    fn quickjs_smoke_eval_returns_json_and_logs() {
        let output = quickjs_smoke_eval(
            r#"
console.log("hello from quickjs");
return { ok: true, value: 42 };
"#,
        )
        .expect("quickjs smoke eval should succeed");

        assert_eq!(output.json_result, r#"{"ok":true,"value":42}"#);
        assert_eq!(output.console_logs, vec!["hello from quickjs".to_string()]);
    }

    #[test]
    fn quickjs_smoke_eval_surfaces_js_exception() {
        let error = quickjs_smoke_eval(
            r#"
throw new Error("boom");
"#,
        )
        .expect_err("quickjs smoke eval should fail");

        assert!(error.message().contains("QuickJS script evaluation failed"));
        assert!(error.to_string().contains("boom"));
    }

    #[test]
    fn quickjs_host_http_wrapper_exposes_sync_client() {
        let output = quickjs_eval_with_bindings(
            r#"
const response = http.post("https://example.com/api", {
    headers: { "x-test": "1" },
    json: { ping: "pong" },
    timeoutMs: 1500,
});
return {
    ok: response.ok,
    status: response.status,
    echoedMethod: response.echoed.method,
    echoedUrl: response.echoed.url,
    echoedBody: response.echoed.json,
};
"#,
            QuickJsHostBindings {
                http_request: Some(Arc::new(|request| {
                    Ok(json!({
                        "ok": true,
                        "status": 200,
                        "echoed": request,
                    }))
                })),
                ..QuickJsHostBindings::default()
            },
        )
        .expect("quickjs host http eval should succeed");

        assert_eq!(
            output.json_result,
            r#"{"ok":true,"status":200,"echoedMethod":"POST","echoedUrl":"https://example.com/api","echoedBody":{"ping":"pong"}}"#
        );
    }

    #[test]
    fn quickjs_host_notify_wrapper_exposes_notification_controls() {
        let output = quickjs_eval_with_bindings(
            r#"
const sent = notify.send({ title: "Reminder", body: "Stretch", category: "health" });
const listed = notify.list({ flat: true, limit: 5 });
const marked = notify.markRead(sent.notification_id);
const categoryMarked = notify.markCategoryRead("health");
const allMarked = notify.markAllRead();
const deleted = notify.deleteCategory("health");
return { sent, listed, marked, categoryMarked, allMarked, deleted };
"#,
            QuickJsHostBindings {
                notify_send: Some(Arc::new(|request| {
                    Ok(json!({
                        "notification_id": 77,
                        "echoed": request,
                    }))
                })),
                notify_list: Some(Arc::new(|request| {
                    Ok(json!({
                        "mode": if request.get("flat").and_then(|value| value.as_bool()).unwrap_or(false) {
                            "flat"
                        } else {
                            "grouped"
                        },
                        "items": [{ "id": 77 }],
                    }))
                })),
                notify_mark_read: Some(Arc::new(|request| Ok(request))),
                notify_mark_category_read: Some(Arc::new(|request| Ok(request))),
                notify_mark_all_read: Some(Arc::new(|_request| Ok(json!({ "read": true })))),
                notify_delete_category: Some(Arc::new(|request| Ok(request))),
                ..QuickJsHostBindings::default()
            },
        )
        .expect("quickjs notify eval should succeed");

        assert!(output.json_result.contains("\"notification_id\":77"));
        assert!(output.json_result.contains("\"mode\":\"flat\""));
        assert!(output.json_result.contains("\"category\":\"health\""));
        assert!(output.json_result.contains("\"read\":true"));
    }

    #[tokio::test]
    async fn quickjs_smoke_handler_runs_through_executor() {
        let mut executor = ExternalToolExecutor::new();
        register_quickjs_external_tools(&mut executor);

        let plan = executor
            .resolve_calls([ExternalToolCallRequest {
                tool_call_id: "quickjs:0".to_string(),
                tool_name: "quickjs_eval".to_string(),
                arguments: json!({
                    "type": "quickjs_eval_request",
                    "script": "console.log(\"executor\"); return { answer: 7 * 6 };"
                }),
            }])
            .await;

        assert!(plan.pending_calls.is_empty());
        assert_eq!(plan.resolved_results.len(), 1);
        assert_eq!(
            plan.resolved_results[0].result.as_deref(),
            Some("{\"console_logs\":[\"executor\"],\"json_result\":\"{\\\"answer\\\":42}\"}")
        );
    }
}
