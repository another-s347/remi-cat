/// Remi CLI — single-shot streaming agent with persistent JSON memory
///
/// Usage:
///   remi-cli --message "Hello"
///   remi-cli --message "Hello" --thread-id <uuid> --memory-file ./memory.json
///
/// Flags:
///   --message   / -m   Message to send (required)
///   --thread-id / -t   Thread ID to continue (creates new thread if omitted)
///   --memory-file / -f Path to JSON memory file (default: remi-memory.json in cwd)
///
/// Env vars:
///   OPENAI_API_KEY  or  REMI_API_KEY    — API key (required)
///   REMI_BASE_URL   or  OPENAI_BASE_URL — custom base URL
///   REMI_MODEL                          — model name (default: gpt-4o)
///   REMI_SYSTEM                         — system prompt
///   REMI_MAX_TURNS                      — max turns per run (default: 20)
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use remi_agentloop::prelude::*;
use remi_agentloop::tool_macro as tool;

// ── Built-in calculator tools ────────────────────────────────────────────────

/// Add two numbers together.
#[tool]
async fn add(a: f64, b: f64) -> f64 {
    a + b
}

/// Subtract b from a.
#[tool]
async fn subtract(a: f64, b: f64) -> f64 {
    a - b
}

/// Multiply two numbers together.
#[tool]
async fn multiply(a: f64, b: f64) -> f64 {
    a * b
}

/// Divide a by b.
#[tool]
async fn divide(a: f64, b: f64) -> String {
    if b == 0.0 {
        "error: division by zero".to_string()
    } else {
        (a / b).to_string()
    }
}

/// Compute integer power: base^exp.
#[tool]
async fn power(base: i64, exp: i64) -> i64 {
    base.pow(exp.max(0) as u32)
}

// ── ANSI helpers ─────────────────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const MAGENTA: &str = "\x1b[35m";
const RED: &str = "\x1b[31m";
const BLUE: &str = "\x1b[34m";

// ── JSON file–backed context store ───────────────────────────────────────────

/// On-disk format stored in the memory JSON file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MemoryFile {
    threads: HashMap<String, Vec<Message>>,
    /// extra metadata: map thread_id → human-readable note
    #[serde(default)]
    meta: HashMap<String, ThreadMeta>,
    /// run_id → thread_id
    #[serde(default)]
    runs: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ThreadMeta {
    created_at: String,
    message_count: usize,
}

/// A `ContextStore` implementation backed by a JSON file.
/// Writes are flushed to disk immediately after every mutation.
#[derive(Clone)]
struct JsonFileStore {
    data: Arc<Mutex<MemoryFile>>,
    path: PathBuf,
}

impl JsonFileStore {
    /// Load an existing file or start with empty data.
    fn load(path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let data = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str::<MemoryFile>(&raw)?
        } else {
            MemoryFile::default()
        };
        Ok(Self {
            data: Arc::new(Mutex::new(data)),
            path,
        })
    }

    /// Persist current in-memory state to disk (async).
    /// The snapshot is taken lazily inside the future (after the mutex is
    /// released by the caller), so it always reflects the latest mutations.
    fn flush(&self) -> impl Future<Output = Result<(), AgentError>> + Send + 'static {
        let data = self.data.clone();
        let path = self.path.clone();
        async move {
            let snapshot = data.lock().unwrap().clone();
            let json = serde_json::to_string_pretty(&snapshot)
                .map_err(|e| AgentError::Store(format!("serialize: {e}")))?;
            tokio::fs::write(&path, json)
                .await
                .map_err(|e| AgentError::Io(format!("write {}: {e}", path.display())))?;
            Ok(())
        }
    }
}

impl ContextStore for JsonFileStore {
    fn create_thread(&self) -> impl Future<Output = Result<ThreadId, AgentError>> {
        let data = self.data.clone();
        let flush = self.flush();
        async move {
            let tid = ThreadId::new();
            {
                let mut guard = data.lock().unwrap();
                guard.threads.insert(tid.0.clone(), vec![]);
                guard.meta.insert(
                    tid.0.clone(),
                    ThreadMeta {
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message_count: 0,
                    },
                );
            }
            flush.await?;
            Ok(tid)
        }
    }

    fn get_messages(
        &self,
        thread_id: &ThreadId,
    ) -> impl Future<Output = Result<Vec<Message>, AgentError>> {
        let data = self.data.clone();
        let tid = thread_id.clone();
        async move {
            let guard = data.lock().unwrap();
            guard
                .threads
                .get(&tid.0)
                .cloned()
                .ok_or(AgentError::ThreadNotFound(tid))
        }
    }

    fn get_recent_messages(
        &self,
        thread_id: &ThreadId,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<Message>, AgentError>> {
        let data = self.data.clone();
        let tid = thread_id.clone();
        async move {
            let guard = data.lock().unwrap();
            let msgs = guard
                .threads
                .get(&tid.0)
                .ok_or(AgentError::ThreadNotFound(tid))?;
            let skip = msgs.len().saturating_sub(limit);
            Ok(msgs[skip..].to_vec())
        }
    }

    fn append_message(
        &self,
        thread_id: &ThreadId,
        message: Message,
    ) -> impl Future<Output = Result<MessageId, AgentError>> {
        let data = self.data.clone();
        let tid = thread_id.clone();
        let flush = self.flush();
        async move {
            let id = {
                let mut guard = data.lock().unwrap();
                let msgs = guard.threads.entry(tid.0.clone()).or_default();
                let id = message.id.clone();
                msgs.push(message);
                let count = msgs.len();
                if let Some(meta) = guard.meta.get_mut(&tid.0) {
                    meta.message_count = count;
                }
                id
            };
            flush.await?;
            Ok(id)
        }
    }

    fn append_messages(
        &self,
        thread_id: &ThreadId,
        messages: Vec<Message>,
    ) -> impl Future<Output = Result<Vec<MessageId>, AgentError>> {
        let data = self.data.clone();
        let tid = thread_id.clone();
        let flush = self.flush();
        async move {
            let ids = {
                let mut guard = data.lock().unwrap();
                let msgs = guard.threads.entry(tid.0.clone()).or_default();
                let ids: Vec<_> = messages.iter().map(|m| m.id.clone()).collect();
                msgs.extend(messages);
                let count = msgs.len();
                if let Some(meta) = guard.meta.get_mut(&tid.0) {
                    meta.message_count = count;
                }
                ids
            };
            flush.await?;
            Ok(ids)
        }
    }

    fn delete_thread(&self, thread_id: &ThreadId) -> impl Future<Output = Result<(), AgentError>> {
        let data = self.data.clone();
        let tid = thread_id.clone();
        let flush = self.flush();
        async move {
            {
                let mut guard = data.lock().unwrap();
                guard.threads.remove(&tid.0);
                guard.meta.remove(&tid.0);
            }
            flush.await?;
            Ok(())
        }
    }

    fn create_run(&self, thread_id: &ThreadId) -> impl Future<Output = Result<RunId, AgentError>> {
        let data = self.data.clone();
        let tid = thread_id.clone();
        let flush = self.flush();
        async move {
            let rid = RunId::new();
            {
                let mut guard = data.lock().unwrap();
                guard.runs.insert(rid.0.clone(), tid.0.clone());
            }
            flush.await?;
            Ok(rid)
        }
    }

    fn complete_run(&self, run_id: &RunId) -> impl Future<Output = Result<(), AgentError>> {
        let data = self.data.clone();
        let rid = run_id.clone();
        let flush = self.flush();
        async move {
            {
                let mut guard = data.lock().unwrap();
                guard.runs.remove(&rid.0);
            }
            flush.await?;
            Ok(())
        }
    }
}

// ── CLI argument parsing ──────────────────────────────────────────────────────

#[derive(Debug)]
struct CliArgs {
    message: String,
    thread_id: Option<String>,
    memory_file: PathBuf,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut args = std::env::args().skip(1);
    let mut message = None;
    let mut thread_id = None;
    let mut memory_file = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--message" | "-m" => {
                message = Some(args.next().ok_or("--message requires a value")?);
            }
            "--thread-id" | "-t" => {
                thread_id = Some(args.next().ok_or("--thread-id requires a value")?);
            }
            "--memory-file" | "-f" => {
                memory_file = Some(PathBuf::from(
                    args.next().ok_or("--memory-file requires a value")?,
                ));
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
    }

    Ok(CliArgs {
        message: message.ok_or("--message / -m is required")?,
        thread_id,
        memory_file: memory_file.unwrap_or_else(|| PathBuf::from("remi-memory.json")),
    })
}

fn print_help() {
    println!(
        r#"remi-cli — streaming agent with persistent JSON memory

USAGE:
  remi-cli --message "Hello" [OPTIONS]

OPTIONS:
  -m, --message   <text>   Message to send (required)
  -t, --thread-id <uuid>   Continue an existing thread (omit to start new)
  -f, --memory-file <path> JSON file for conversation history
                           (default: remi-memory.json in current directory)
  -h, --help               Print this help

ENV VARS:
  OPENAI_API_KEY / REMI_API_KEY  — API key (required)
  REMI_BASE_URL / OPENAI_BASE_URL — custom base URL
  REMI_MODEL                     — model name (default: gpt-4o)
  REMI_SYSTEM                    — system prompt
  REMI_MAX_TURNS                 — max agent turns per run (default: 20)
"#
    );
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Parse args ────────────────────────────────────────────────────────
    let args = parse_args().unwrap_or_else(|e| {
        eprintln!("{RED}error:{RESET} {e}");
        eprintln!("Run with --help for usage.");
        std::process::exit(1);
    });

    // ── Env config ────────────────────────────────────────────────────────
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("REMI_API_KEY"))
        .unwrap_or_default();

    if api_key.is_empty() {
        eprintln!("{RED}error:{RESET} OPENAI_API_KEY or REMI_API_KEY is required.");
        std::process::exit(1);
    }

    let base_url = std::env::var("REMI_BASE_URL")
        .or_else(|_| std::env::var("OPENAI_BASE_URL"))
        .ok();
    let model_name = std::env::var("REMI_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
    let system_prompt = std::env::var("REMI_SYSTEM").unwrap_or_default();
    let max_turns: usize = std::env::var("REMI_MAX_TURNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    // ── Load memory file ──────────────────────────────────────────────────
    let store = JsonFileStore::load(args.memory_file.clone()).unwrap_or_else(|e| {
        eprintln!(
            "{RED}error:{RESET} cannot load memory file {}: {e}",
            args.memory_file.display()
        );
        std::process::exit(1);
    });
    // ── Resolve or create thread ──────────────────────────────────────────
    let thread_id = if let Some(tid_str) = args.thread_id {
        let tid = ThreadId(tid_str);
        // Ensure thread exists in store; create it if not.
        let exists = store.data.lock().unwrap().threads.contains_key(&tid.0);
        if !exists {
            {
                let mut guard = store.data.lock().unwrap();
                guard.threads.insert(tid.0.clone(), vec![]);
                guard.meta.insert(
                    tid.0.clone(),
                    ThreadMeta {
                        created_at: chrono::Utc::now().to_rfc3339(),
                        message_count: 0,
                    },
                );
            }
            store.flush().await?;
        }
        tid
    } else {
        store.create_thread().await?
    };

    // ── Build agent ───────────────────────────────────────────────────────
    let mut oai = OpenAIClient::new(api_key).with_model(model_name.clone());
    if let Some(url) = base_url {
        oai = oai.with_base_url(url);
    }

    let mut builder = AgentBuilder::new()
        .model(oai)
        .context_store(store)
        .max_turns(max_turns);
    if !system_prompt.is_empty() {
        builder = builder.system(system_prompt);
    }

    // ── Register tools (enabled via REMI_TOOLS env var) ───────────────────
    let tools_env = std::env::var("REMI_TOOLS").unwrap_or_default();
    let enable_tools = !tools_env.is_empty() && tools_env != "0" && tools_env != "false";
    if enable_tools {
        builder = builder
            .tool(Add::new())
            .tool(Subtract::new())
            .tool(Multiply::new())
            .tool(Divide::new())
            .tool(Power::new());
        eprintln!("{DIM}tools:{RESET} add, subtract, multiply, divide, power");
    }

    // ── Optional tracing ──────────────────────────────────────────────────
    let trace_env = std::env::var("REMI_TRACE").unwrap_or_default();
    let enable_trace = !trace_env.is_empty() && trace_env != "0" && trace_env != "false";
    if enable_trace {
        builder = builder.tracer(StdoutTracer);
        eprintln!("{DIM}tracing:{RESET} stdout (enabled)");
    }

    let agent = builder.build();

    // ── Print header ──────────────────────────────────────────────────────
    eprintln!(
        "{DIM}thread:{RESET} {CYAN}{}{RESET}  {DIM}model:{RESET} {model_name}  {DIM}memory:{RESET} {}",
        thread_id,
        args.memory_file.display()
    );
    eprintln!("{DIM}{}{RESET}", "─".repeat(60));
    eprintln!("{BOLD}{CYAN}>{RESET} {BOLD}{}{RESET}", args.message);
    eprintln!("{DIM}{}{RESET}", "─".repeat(60));

    // ── Run agent ─────────────────────────────────────────────────────────
    let run_start = Instant::now();
    let stream = agent
        .chat_in_thread(&thread_id, args.message)
        .await
        .unwrap_or_else(|e| {
            eprintln!("{RED}error:{RESET} {e}");
            std::process::exit(1);
        });

    let mut stream = std::pin::pin!(stream);
    let mut total_prompt_tokens: u32 = 0;
    let mut total_completion_tokens: u32 = 0;
    let mut current_tool: Option<(String, String, Instant)> = None; // (id, name, started)
    let mut turn = 1usize; // first turn is always 1; TurnStart only fires for turns ≥ 2
    let mut printed_newline = false;
    let mut assistant_content = String::new(); // collected for persistence

    while let Some(event) = stream.next().await {
        match event {
            AgentEvent::RunStart { .. } => {}

            AgentEvent::TurnStart { turn: t } => {
                turn = t;
                if t > 1 {
                    if !printed_newline {
                        println!();
                    }
                    eprintln!("{DIM}─── turn {t} ───{RESET}");
                    printed_newline = false;
                }
            }

            AgentEvent::TextDelta(text) => {
                print!("{text}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                printed_newline = text.ends_with('\n');
                assistant_content.push_str(&text);
            }

            AgentEvent::ThinkingStart => {
                // Thinking models: silently ignore in non-interactive CLI.
            }

            AgentEvent::ThinkingEnd { .. } => {
                // Full reasoning content available but not displayed in CLI mode.
            }

            AgentEvent::ToolCallStart { id, name } => {
                if !printed_newline {
                    println!();
                }
                eprint!("{YELLOW}⚙ {BOLD}{name}{RESET}{YELLOW}(");
                current_tool = Some((id, name, Instant::now()));
                printed_newline = false;
            }

            AgentEvent::ToolCallArgumentsDelta { delta, .. } => {
                eprint!("{DIM}{delta}{RESET}");
            }

            AgentEvent::ToolDelta { delta, .. } => {
                let _ = delta;
            }

            AgentEvent::ToolResult { id, result, .. } => {
                let elapsed = current_tool
                    .as_ref()
                    .filter(|(cid, _, _)| *cid == id)
                    .map(|(_, _, started)| started.elapsed());

                let elapsed_str = elapsed
                    .map(|d| format!("{:.2}s", d.as_secs_f64()))
                    .unwrap_or_default();

                let display_result = if result.chars().count() > 200 {
                    format!(
                        "{}…  [{} chars]",
                        result.chars().take(200).collect::<String>(),
                        result.len()
                    )
                } else {
                    result.clone()
                };

                eprintln!("){RESET}");
                eprintln!(
                    "  {GREEN}→ {RESET}{DIM}{}{RESET}  {DIM}[{elapsed_str}]{RESET}",
                    display_result.lines().next().unwrap_or("(no output)")
                );

                if current_tool
                    .as_ref()
                    .map(|(cid, _, _)| cid == &id)
                    .unwrap_or(false)
                {
                    current_tool = None;
                }
                printed_newline = true;
            }

            AgentEvent::Interrupt { interrupts } => {
                eprintln!();
                for info in &interrupts {
                    eprintln!(
                        "{MAGENTA}{BOLD}⚠ INTERRUPT:{RESET} tool={MAGENTA}{}{RESET}  kind={}",
                        info.tool_name, info.kind
                    );
                }
                eprintln!("{DIM}(interrupt not handled in CLI mode — aborting run){RESET}");
                break;
            }

            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => {
                total_prompt_tokens += prompt_tokens;
                total_completion_tokens += completion_tokens;
            }

            AgentEvent::Error(e) => {
                eprintln!("\n{RED}error:{RESET} {e}");
            }

            AgentEvent::Done => {
                if !printed_newline {
                    println!();
                }

                let elapsed = run_start.elapsed();
                let total_tokens = total_prompt_tokens + total_completion_tokens;
                eprintln!("{DIM}{}{RESET}", "─".repeat(60));
                eprintln!(
                    "{DIM}turns:{RESET} {turn}  \
                     {DIM}tokens:{RESET} {BLUE}{total_tokens}{RESET} \
                     {DIM}(↑{total_prompt_tokens} ↓{total_completion_tokens}){RESET}  \
                     {DIM}elapsed:{RESET} {GREEN}{:.2}s{RESET}",
                    elapsed.as_secs_f64()
                );
                eprintln!(
                    "{DIM}thread:{RESET} {CYAN}{thread_id}{RESET}  \
                     {DIM}memory:{RESET} {}",
                    args.memory_file.display()
                );
                break;
            }

            // Internal events — shouldn't reach us, but ignore if they do.
            AgentEvent::NeedToolExecution { .. } => {}
            AgentEvent::Cancelled => {}
            AgentEvent::Checkpoint(_) => {}
        }
    }

    Ok(())
}
