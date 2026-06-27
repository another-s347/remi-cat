import {
  ComposerPrimitive,
  MessagePrimitive,
  ThreadPrimitive,
  type ReasoningMessagePartProps,
  type TextMessagePartProps,
  type ToolCallMessagePartProps,
  useComposer,
  useComposerRuntime,
} from "@assistant-ui/react";
import {
  Activity,
  Bot,
  CheckCircle2,
  Circle,
  Command,
  FileText,
  GitBranch,
  KeyRound,
  ListTodo,
  Menu,
  Pencil,
  Plus,
  Send,
  Square,
  Trash2,
  User,
  Wrench,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  api,
  type ActiveRun,
  type CommandCatalogEntry,
  type ContextCompactionEvent,
  type DebugStats,
  type HistoryMessage,
  type ModelInputCategory,
  type ModelInputSnapshot,
  type ModelInputSummary,
  type PrettyToolCall,
  type SecretList,
  type Session,
  type ToolApprovalDecision,
  type ToolApprovalRequest,
  type TodoItem,
  type UserQuestionRequest,
  type UserQuestionResponse,
  type WorkspaceFileMatch,
} from "./api";
import {
  activeFileMentionToken,
  CHAT_COMMANDS,
  commandSuggestions,
  replaceFileMentionToken,
  type ChatCommand,
  type FileMentionToken,
} from "./commands";
import { RemiRuntimeProvider } from "./runtime";

function TextPart({ text }: TextMessagePartProps) {
  return <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>;
}

function ReasoningPart({ text }: ReasoningMessagePartProps) {
  return (
    <details className="reasoning">
      <summary>思考过程</summary>
      <div>{text}</div>
    </details>
  );
}

function prettyFromPart(args: unknown, result: unknown): PrettyToolCall | undefined {
  const resultPretty = (result as { pretty?: PrettyToolCall } | undefined)?.pretty;
  if (resultPretty) return resultPretty;
  return (args as { pretty?: PrettyToolCall } | undefined)?.pretty;
}

function workspaceImagePreview(pretty: PrettyToolCall) {
  if (pretty.tool_name !== "fs_read") return undefined;
  const path = (pretty.request as { path?: unknown } | undefined)?.path;
  if (typeof path !== "string" || !isPreviewableImagePath(path)) return undefined;
  return {
    path,
    url: `/api/v1/chat/assets/workspace/${encodeWorkspacePath(path)}`,
  };
}

function isPreviewableImagePath(path: string) {
  return /\.(png|jpe?g|gif|webp)$/i.test(path);
}

function encodeWorkspacePath(path: string) {
  return path
    .split("/")
    .filter((part) => part.length > 0)
    .map(encodeURIComponent)
    .join("/");
}

function formatFileSize(size?: number) {
  if (size === undefined) return "文件";
  if (size >= 1024 * 1024) return `${(size / (1024 * 1024)).toFixed(1)} MB`;
  if (size >= 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${size} B`;
}

type PatchLine = {
  kind: "add" | "remove" | "context" | "meta";
  text: string;
};

type PatchFile = {
  path: string;
  oldPath?: string;
  newPath?: string;
  operation?: "add" | "delete" | "update";
  lines: PatchLine[];
  added: number;
  removed: number;
};

function patchTextFromPretty(pretty: PrettyToolCall) {
  const request = pretty.request as { patch?: unknown } | undefined;
  return typeof request?.patch === "string" ? request.patch : undefined;
}

function normalizeDiffPath(path: string) {
  const clean = path.trim().split(/\s+/)[0] ?? "";
  if (clean === "/dev/null") return clean;
  return clean.replace(/^([ab])\//, "");
}

function parsePatchFiles(patch: string): PatchFile[] {
  return patch.includes("*** Begin Patch") || patch.includes("*** Update File:")
    ? parseCodexPatchFiles(patch)
    : parseUnifiedPatchFiles(patch);
}

function parseUnifiedPatchFiles(patch: string): PatchFile[] {
  const files: PatchFile[] = [];
  let current: PatchFile | undefined;
  let pendingOldPath: string | undefined;
  const ensureCurrent = (path: string, oldPath?: string, newPath?: string) => {
    current = {
      path,
      oldPath,
      newPath,
      operation: oldPath === "/dev/null" ? "add" : newPath === "/dev/null" ? "delete" : "update",
      lines: [],
      added: 0,
      removed: 0,
    };
    files.push(current);
  };

  for (const rawLine of patch.split(/\r?\n/)) {
    if (rawLine.startsWith("diff --git ")) {
      const parts = rawLine.trim().split(/\s+/);
      pendingOldPath = normalizeDiffPath(parts[2] ?? "");
      const newPath = normalizeDiffPath(parts[3] ?? pendingOldPath ?? "patch");
      ensureCurrent(newPath, pendingOldPath, newPath);
      current?.lines.push({ kind: "meta", text: rawLine });
      continue;
    }
    if (rawLine.startsWith("--- ")) {
      pendingOldPath = normalizeDiffPath(rawLine.slice(4));
      if (current) current.oldPath = pendingOldPath;
      continue;
    }
    if (rawLine.startsWith("+++ ")) {
      const newPath = normalizeDiffPath(rawLine.slice(4));
      if (!current || current.lines.some((line) => line.text.startsWith("@@"))) {
        ensureCurrent(newPath === "/dev/null" ? pendingOldPath ?? newPath : newPath, pendingOldPath, newPath);
      } else {
        current.path = newPath === "/dev/null" ? pendingOldPath ?? newPath : newPath;
        current.newPath = newPath;
        current.operation = pendingOldPath === "/dev/null" ? "add" : newPath === "/dev/null" ? "delete" : "update";
      }
      continue;
    }
    if (!current) continue;
    const line = classifyPatchLine(rawLine);
    current.lines.push(line);
    if (line.kind === "add") current.added += 1;
    if (line.kind === "remove") current.removed += 1;
  }
  return files.filter((file) => file.lines.length > 0);
}

function parseCodexPatchFiles(patch: string): PatchFile[] {
  const files: PatchFile[] = [];
  let current: PatchFile | undefined;
  for (const rawLine of patch.split(/\r?\n/)) {
    const updatePath = rawLine.match(/^\*\*\* Update File:\s+(.+)$/)?.[1];
    const addPath = rawLine.match(/^\*\*\* Add File:\s+(.+)$/)?.[1];
    const deletePath = rawLine.match(/^\*\*\* Delete File:\s+(.+)$/)?.[1];
    if (updatePath || addPath || deletePath) {
      current = {
        path: (updatePath ?? addPath ?? deletePath ?? "patch").trim(),
        operation: addPath ? "add" : deletePath ? "delete" : "update",
        lines: [],
        added: 0,
        removed: 0,
      };
      files.push(current);
      continue;
    }
    if (!current || rawLine === "*** Begin Patch" || rawLine === "*** End Patch") continue;
    const line = classifyPatchLine(rawLine);
    current.lines.push(line);
    if (line.kind === "add") current.added += 1;
    if (line.kind === "remove") current.removed += 1;
  }
  return files.filter((file) => file.lines.length > 0);
}

function classifyPatchLine(rawLine: string): PatchLine {
  if (rawLine.startsWith("+") && !rawLine.startsWith("+++")) {
    return { kind: "add", text: rawLine };
  }
  if (rawLine.startsWith("-") && !rawLine.startsWith("---")) {
    return { kind: "remove", text: rawLine };
  }
  if (rawLine.startsWith("@@") || rawLine.startsWith("***") || rawLine.startsWith("index ")) {
    return { kind: "meta", text: rawLine };
  }
  return { kind: "context", text: rawLine };
}

function PatchDiffViewer({ patch }: { patch: string }) {
  const files = parsePatchFiles(patch);
  if (!files.length) {
    return (
      <details className="patch-viewer">
        <summary>Patch</summary>
        <pre>{patch}</pre>
      </details>
    );
  }
  const added = files.reduce((sum, file) => sum + file.added, 0);
  const removed = files.reduce((sum, file) => sum + file.removed, 0);
  return (
    <div className="patch-viewer">
      <div className="patch-summary">
        <span>{files.length} file{files.length === 1 ? "" : "s"}</span>
        <strong className="patch-added">+{added}</strong>
        <strong className="patch-removed">-{removed}</strong>
      </div>
      {files.map((file, index) => (
        <details className="patch-file" key={`${file.path}-${index}`} open>
          <summary>
            <code>{file.path}</code>
            <small>{file.operation ?? "update"} · +{file.added} / -{file.removed}</small>
          </summary>
          <pre>
            {file.lines.map((line, lineIndex) => (
              <span className={`patch-line ${line.kind}`} key={lineIndex}>
                {line.text || " "}
                {"\n"}
              </span>
            ))}
          </pre>
        </details>
      ))}
    </div>
  );
}

function ApprovalPart({ args, result }: ToolCallMessagePartProps) {
  const payload = (result ?? args) as {
    request?: ToolApprovalRequest;
    decision?: ToolApprovalDecision;
  };
  const request = payload.request;
  const [pending, setPending] = useState<ToolApprovalDecision | null>(null);
  if (!request) return null;
  const decision = payload.decision;
  const disabled = Boolean(decision || pending);
  const review = request.review;
  const submit = async (next: ToolApprovalDecision) => {
    setPending(next);
    try {
      await api.decideApproval(request.id, next);
    } catch {
      setPending(null);
    }
  };
  return (
    <section className={`approval-card risk-${review?.risk ?? request.risk}`}>
      <header>
        <Wrench size={15} />
        <strong>Tool approval</strong>
        <span>{request.tool_name}</span>
        <small>{review?.risk ?? request.risk}</small>
      </header>
      <p>{review?.reason ?? "This tool needs approval before it can run."}</p>
      {review?.concerns?.length ? (
        <ul>
          {review.concerns.map((concern) => <li key={concern}>{concern}</li>)}
        </ul>
      ) : null}
      <details>
        <summary>参数摘要</summary>
        <pre>{request.args_summary}</pre>
      </details>
      {decision ? (
        <div className="approval-status">Decision: {decision.replaceAll("_", " ")}</div>
      ) : (
        <div className="approval-actions">
          <button type="button" disabled={disabled} onClick={() => submit("allow_once")}>
            Allow once
          </button>
          <button type="button" disabled={disabled} onClick={() => submit("allow_same_command_session")}>
            Always allow same command
          </button>
          <button type="button" disabled={disabled} onClick={() => submit("allow_risk_level_session")}>
            Always allow this level (low/medium)
          </button>
          <button type="button" disabled={disabled} className="danger" onClick={() => submit("deny")}>
            Deny
          </button>
        </div>
      )}
    </section>
  );
}

function UserQuestionPart({ args, result }: ToolCallMessagePartProps) {
  const payload = (result ?? args) as {
    request?: UserQuestionRequest;
    response?: UserQuestionResponse;
  };
  const request = payload.request;
  const response = payload.response;
  const [text, setText] = useState("");
  const [pending, setPending] = useState(false);
  if (!request) return null;
  const disabled = Boolean(response || pending);
  const submit = async (selectedOptionIds: string[] = [], cancel = false) => {
    setPending(true);
    try {
      await api.answerUserQuestion(request.id, {
        selected_option_ids: selectedOptionIds,
        free_text: text.trim() || null,
        source: "web",
        cancel,
      });
    } catch {
      setPending(false);
    }
  };
  return (
    <section className="approval-card user-question-card">
      <header>
        <Wrench size={15} />
        <strong>Question</strong>
        <small>{response?.status ?? "waiting"}</small>
      </header>
      <p>{request.question}</p>
      {request.reason ? <p>{request.reason}</p> : null}
      {response ? (
        <div className="approval-status">
          {response.answer_text ?? response.status}
        </div>
      ) : (
        <>
          {request.allow_free_text ? (
            <textarea
              value={text}
              disabled={disabled}
              placeholder={request.placeholder ?? "Add details"}
              onChange={(event) => setText(event.target.value)}
            />
          ) : null}
          <div className="approval-actions">
            {request.options.map((option) => (
              <button
                type="button"
                disabled={disabled}
                key={option.id}
                onClick={() => submit([option.id])}
              >
                {option.label}
              </button>
            ))}
            {request.allow_free_text ? (
              <button type="button" disabled={disabled} onClick={() => submit()}>
                Submit
              </button>
            ) : null}
            <button type="button" disabled={disabled} className="danger" onClick={() => submit([], true)}>
              Cancel
            </button>
          </div>
        </>
      )}
    </section>
  );
}

function ContextCompactionPart({ args, result }: ToolCallMessagePartProps) {
  const event = (result ?? args) as ContextCompactionEvent | undefined;
  if (!event) return null;
  const title = event.status === "failed"
    ? "上下文压缩失败"
    : event.status === "completed"
      ? "上下文已压缩"
      : "正在压缩上下文";
  const detail = event.status === "failed" && event.error
    ? event.error
    : `压缩 ${event.compacted_messages} 条，保留 ${event.remaining_messages} 条`;
  return (
    <section className={`compaction-card ${event.status}`}>
      <header>
        <Activity size={15} />
        <strong>{title}</strong>
        <small>{event.source}</small>
      </header>
      <p>{detail}</p>
    </section>
  );
}

function ToolPart({ toolName, args, argsText, result, status }: ToolCallMessagePartProps) {
  const pretty = prettyFromPart(args, result);
  if (pretty) {
    const patchText = pretty.tool_name === "apply_patch" ? patchTextFromPretty(pretty) : undefined;
    const icon = pretty.status === "error" ? "❌" : pretty.status === "success" ? "✅" : "⏳";
    const liveElapsedMs =
      pretty.elapsed_ms ??
      (pretty.status === "running" && pretty.started_at_ms != null
        ? performance.now() - pretty.started_at_ms
        : null);
    const elapsedLabel = liveElapsedMs == null
      ? pretty.status === "running" ? "执行中" : "已完成"
      : formatDuration(liveElapsedMs);
    const imagePreview = workspaceImagePreview(pretty);
    return (
      <details className={`tool pretty-tool ${pretty.status}`} open={status.type === "running"}>
        <summary>
          <Wrench size={15} />
          <span>{icon} {pretty.title}</span>
          <small>{elapsedLabel}</small>
        </summary>
        <p>{pretty.summary}</p>
        {imagePreview && pretty.status === "success" && (
          <figure className="tool-image-preview">
            <img src={imagePreview.url} alt={imagePreview.path} loading="lazy" />
            <figcaption>{imagePreview.path}</figcaption>
          </figure>
        )}
        {patchText && (
          <PatchDiffViewer patch={patchText} />
        )}
        {pretty.tool_name !== "apply_patch" && (
          <details>
            <summary>完整 request</summary>
            <pre>{JSON.stringify(pretty.request, null, 2)}</pre>
          </details>
        )}
        {pretty.response !== undefined && pretty.response !== null && (
          <details>
            <summary>完整 response</summary>
            <pre>{pretty.response}</pre>
          </details>
        )}
      </details>
    );
  }
  return (
    <details className="tool" open={status.type === "running"}>
      <summary><Wrench size={15} /> {toolName}</summary>
      {argsText && <pre>{argsText}</pre>}
      {result !== undefined && <pre>{String(result)}</pre>}
    </details>
  );
}

type SupervisorEvent = {
  type?: string;
  content?: string;
  name?: string;
  args?: unknown;
  result?: string;
};

type SupervisorReport = {
  workflow_name?: string;
  from_node?: string;
  to_node?: string;
  edge?: string | null;
  status?: string;
  reason?: string;
  round?: number;
};

type SubSessionEvent = {
  sub_type?: string;
  agent_name?: string;
  title?: string | null;
  id?: string;
  name?: string;
  content?: string;
  delta?: string;
  result?: string;
  final_output?: string | null;
  message?: string;
  turn?: number;
};

type SubSessionArgs = {
  agentName?: string;
  title?: string | null;
  threadId?: string;
  runId?: string;
  events?: SubSessionEvent[];
};

function SupervisorPart({ args, result, status }: ToolCallMessagePartProps) {
  const events = ((args as { events?: SupervisorEvent[] }).events ?? []);
  const report = result as SupervisorReport | undefined;
  return (
    <details className="supervisor" open={status.type === "running"}>
      <summary>
        <GitBranch size={15} />
        <span>Supervisor</span>
        <small>{report ? `${report.from_node} → ${report.to_node}` : "执行中"}</small>
      </summary>
      <div className="supervisor-body">
        {events.map((event, index) => (
          <div className={`supervisor-event ${event.type ?? "event"}`} key={index}>
            <strong>{event.type?.replaceAll("_", " ") ?? "event"}</strong>
            {event.name && <code>{event.name}</code>}
            {event.content && <pre>{event.content}</pre>}
            {event.args !== undefined && <pre>{JSON.stringify(event.args, null, 2)}</pre>}
            {event.result !== undefined && <pre>{event.result}</pre>}
          </div>
        ))}
        {report && (
          <div className="supervisor-report">
            <div><span>Workflow</span><strong>{report.workflow_name ?? "Supervisor"}</strong></div>
            <div><span>Transition</span><strong>{report.from_node} → {report.to_node}</strong></div>
            <div><span>Status</span><strong>{report.status}</strong></div>
            <div><span>Round</span><strong>{report.round ?? 0}</strong></div>
            {report.edge && <div><span>Edge</span><strong>{report.edge}</strong></div>}
            <p>{report.reason}</p>
          </div>
        )}
      </div>
    </details>
  );
}

function subSessionLabel(event: SubSessionEvent) {
  const type = event.sub_type ?? "event";
  switch (type) {
    case "start":
      return "start";
    case "delta":
      return "output";
    case "thinking_start":
      return "thinking";
    case "thinking_end":
      return "thinking done";
    case "tool_call_start":
      return `tool ${event.name ?? ""}`.trim();
    case "tool_call_arguments_delta":
      return "tool args";
    case "tool_delta":
      return `tool ${event.name ?? ""}`.trim();
    case "tool_result":
      return `tool result ${event.name ?? ""}`.trim();
    case "turn_start":
      return `turn ${event.turn ?? ""}`.trim();
    case "done":
      return "done";
    case "error":
      return "error";
    default:
      return type.replaceAll("_", " ");
  }
}

function isSubSessionActivity(event: SubSessionEvent) {
  const type = event.sub_type ?? "event";
  return type !== "start";
}

function subSessionEventBody(event: SubSessionEvent) {
  if (event.content) return event.content;
  if (event.delta) return event.delta;
  if (event.result !== undefined) return event.result;
  if (event.final_output) return event.final_output;
  if (event.message) return event.message;
  return undefined;
}

function SubSessionPart({ args, result, status }: ToolCallMessagePartProps) {
  const payload = args as SubSessionArgs;
  const events = payload.events ?? [];
  const recentEvents = events.filter(isSubSessionActivity).slice(-3);
  const final = result as SubSessionEvent | undefined;
  const title = payload.title || payload.agentName || "Sub-agent";
  const state = final?.sub_type === "error" ? "failed" : final ? "done" : "running";
  return (
    <details className={`supervisor sub-session ${state}`} open={status.type === "running" || !final}>
      <summary>
        <Bot size={15} />
        <span>{title}</span>
        <small>{payload.threadId ? String(payload.threadId).slice(0, 8) : state}</small>
      </summary>
      <div className="supervisor-body">
        {recentEvents.map((event, index) => (
          <div className={`supervisor-event ${event.sub_type ?? "event"}`} key={index}>
            <strong>{subSessionLabel(event)}</strong>
            {event.id && <code>{event.id}</code>}
            {subSessionEventBody(event) && <pre>{subSessionEventBody(event)}</pre>}
          </div>
        ))}
      </div>
    </details>
  );
}

function formatDuration(value?: number | null) {
  if (value === undefined || value === null) return "N/A";
  return value < 1000 ? `${value} ms` : `${(value / 1000).toFixed(2)} s`;
}

function formatTokens(value?: number) {
  return (value ?? 0).toLocaleString();
}

function DebugPanel({ stats, onOpenContext }: { stats?: DebugStats; onOpenContext: () => void }) {
  if (!stats) return null;
  const contextPercent = Math.min(100, Math.max(0, (stats.context_usage ?? 0) * 100));
  return (
    <details className="debug fixed-panel">
      <summary><Activity size={14} /> Debug <small>{formatDuration(stats.elapsed_ms)}</small></summary>
      <div className="debug-grid">
        <div><span>TTFT</span><strong>{formatDuration(stats.ttft_ms)}</strong></div>
        <div><span>Elapsed</span><strong>{formatDuration(stats.elapsed_ms)}</strong></div>
        <div><span>Model time</span><strong>{formatDuration(stats.model_elapsed_ms)}</strong></div>
        <div><span>Total tokens</span><strong>{formatTokens(stats.total_tokens)}</strong></div>
        <div><span>Input</span><strong>{formatTokens(stats.prompt_tokens)}</strong></div>
        <div><span>Output</span><strong>{formatTokens(stats.completion_tokens)}</strong></div>
      </div>
      <div className="context-usage">
        <span>Context</span>
        <strong>{formatTokens(stats.max_prompt_tokens)} / {formatTokens(stats.context_tokens)} ({contextPercent.toFixed(1)}%)</strong>
        <div><i style={{ width: `${contextPercent}%` }} /></div>
        <button type="button" onClick={onOpenContext}>查看完整上下文</button>
      </div>
    </details>
  );
}

const partComponents = {
  Text: TextPart,
  Reasoning: ReasoningPart,
  tools: {
    by_name: {
      __remi_supervisor: SupervisorPart,
      __remi_sub_session: SubSessionPart,
      __remi_approval: ApprovalPart,
      __remi_user_question: UserQuestionPart,
      __remi_context_compaction: ContextCompactionPart,
    },
    Fallback: ToolPart,
  },
};

function normalizeCommandCatalog(commands: CommandCatalogEntry[]): ChatCommand[] {
  return commands.map((command) => ({
    value: command.value,
    label: command.label,
    description: command.description,
    keywords: command.keywords,
    acceptsArguments: command.accepts_arguments,
  }));
}

function UserMessage() {
  return (
    <MessagePrimitive.Root className="message user-message">
      <div className="avatar"><User size={16} /></div>
      <div className="bubble"><MessagePrimitive.Parts components={partComponents} /></div>
    </MessagePrimitive.Root>
  );
}

function AssistantMessage() {
  return (
    <MessagePrimitive.Root className="message assistant-message">
      <div className="avatar remi"><Bot size={17} /></div>
      <div className="bubble"><MessagePrimitive.Parts components={partComponents} /></div>
    </MessagePrimitive.Root>
  );
}

function CommandComposer({ sessionId }: { sessionId: string }) {
  const composer = useComposerRuntime();
  const text = useComposer((state) => state.text);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const draftBeforeHistoryRef = useRef("");
  const [commandCatalog, setCommandCatalog] = useState<ChatCommand[]>(CHAT_COMMANDS);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [dismissedText, setDismissedText] = useState<string>();
  const [inputHistory, setInputHistory] = useState<string[]>([]);
  const [historyIndex, setHistoryIndex] = useState<number | null>(null);
  const [fileToken, setFileToken] = useState<FileMentionToken>();
  const [fileSuggestions, setFileSuggestions] = useState<WorkspaceFileMatch[]>([]);
  const [fileSelectedIndex, setFileSelectedIndex] = useState(0);
  const suggestions = useMemo(
    () => (fileToken || dismissedText === text ? [] : commandSuggestions(text, commandCatalog).slice(0, 8)),
    [commandCatalog, dismissedText, fileToken, text],
  );

  useEffect(() => {
    let cancelled = false;
    api.commands()
      .then((commands) => {
        if (!cancelled) setCommandCatalog(normalizeCommandCatalog(commands));
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    setSelectedIndex(0);
    setFileSelectedIndex(0);
  }, [text]);

  useEffect(() => {
    const token = activeFileMentionToken(text, inputRef.current?.selectionStart ?? text.length);
    setFileToken(token);
  }, [text]);

  useEffect(() => {
    if (!fileToken) {
      setFileSuggestions([]);
      return;
    }
    let cancelled = false;
    api.fileMatches(fileToken.query, 8)
      .then((response) => {
        if (!cancelled) setFileSuggestions(response.items);
      })
      .catch(() => {
        if (!cancelled) setFileSuggestions([]);
      });
    return () => {
      cancelled = true;
    };
  }, [fileToken?.query, fileToken?.start, fileToken?.end]);

  useEffect(() => {
    let cancelled = false;
    api.inputHistory(sessionId)
      .then((history) => {
        if (!cancelled) setInputHistory(history.items);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [sessionId]);

  const refreshFileToken = useCallback(() => {
    const token = activeFileMentionToken(text, inputRef.current?.selectionStart ?? text.length);
    setFileToken(token);
  }, [text]);

  const complete = useCallback(
    (index: number) => {
      const command = suggestions[index];
      if (!command) return;
      composer.setText(command.value);
      setHistoryIndex(null);
      setDismissedText(undefined);
      requestAnimationFrame(() => inputRef.current?.focus());
    },
    [composer, suggestions],
  );

  const completeFile = useCallback(
    (index: number) => {
      const token = fileToken;
      const file = fileSuggestions[index];
      if (!token || !file) return;
      composer.setText(replaceFileMentionToken(text, token, file.mention_path));
      setFileToken(undefined);
      setFileSuggestions([]);
      setHistoryIndex(null);
      requestAnimationFrame(() => inputRef.current?.focus());
    },
    [composer, fileSuggestions, fileToken, text],
  );

  const rememberInput = useCallback((value: string) => {
    const normalized = value.trim();
    if (!normalized) return;
    setInputHistory((history) => {
      const updated = history.at(-1) === normalized ? history : [...history, normalized];
      return updated.slice(-100);
    });
    setHistoryIndex(null);
    draftBeforeHistoryRef.current = "";
    void api.appendInputHistory(sessionId, normalized)
      .then((history) => setInputHistory(history.items))
      .catch(() => undefined);
  }, [sessionId]);

  const recallInputHistory = useCallback(
    (direction: -1 | 1) => {
      if (inputHistory.length === 0) return;
      const current = historyIndex ?? inputHistory.length;
      if (historyIndex === null) {
        draftBeforeHistoryRef.current = text;
      }
      const next = direction < 0
        ? Math.max(0, current - 1)
        : Math.min(inputHistory.length, current + 1);
      setHistoryIndex(next < inputHistory.length ? next : null);
      composer.setText(next < inputHistory.length ? inputHistory[next] : draftBeforeHistoryRef.current);
      requestAnimationFrame(() => inputRef.current?.focus());
    },
    [composer, historyIndex, inputHistory, text],
  );

  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    const token = activeFileMentionToken(text, event.currentTarget.selectionStart ?? text.length);
    if (token?.start !== fileToken?.start || token?.end !== fileToken?.end || token?.query !== fileToken?.query) {
      setFileToken(token);
    }
    if (event.altKey && event.key === "ArrowUp") {
      event.preventDefault();
      recallInputHistory(-1);
      return;
    }
    if (event.altKey && event.key === "ArrowDown") {
      event.preventDefault();
      recallInputHistory(1);
      return;
    }
    if (fileToken && fileSuggestions.length > 0) {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        setFileToken(undefined);
        setFileSuggestions([]);
        return;
      }
      if (event.key === "ArrowUp") {
        event.preventDefault();
        setFileSelectedIndex((index) => Math.max(0, index - 1));
        return;
      }
      if (event.key === "ArrowDown") {
        event.preventDefault();
        setFileSelectedIndex((index) => Math.min(fileSuggestions.length - 1, index + 1));
        return;
      }
      if (event.key === "Tab" || (event.key === "Enter" && !event.shiftKey)) {
        event.preventDefault();
        completeFile(fileSelectedIndex);
        return;
      }
    }
    if (suggestions.length > 0) {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        setDismissedText(text);
        return;
      }
      if (event.key === "Tab") {
        event.preventDefault();
        complete(selectedIndex);
        return;
      }
      if (event.key === "Enter" && !event.shiftKey) {
        const selected = suggestions[selectedIndex];
        if (selected && (selected.acceptsArguments || text.trim() !== selected.value.trim())) {
          event.preventDefault();
          complete(selectedIndex);
          return;
        }
      }
    }
    if (event.key === "Enter" && !event.shiftKey) {
      rememberInput(text);
    }
  };

  return (
    <div className="composer-stack">
      {suggestions.length > 0 && (
        <div className="command-menu" role="listbox" aria-label="命令建议">
          <div className="command-menu-header"><Command size={14} /> 命令</div>
          {suggestions.map((command, index) => (
            <button
              className={index === selectedIndex ? "command-item selected" : "command-item"}
              key={command.value}
              role="option"
              aria-selected={index === selectedIndex}
              onMouseEnter={() => setSelectedIndex(index)}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => complete(index)}
              type="button"
            >
              <code>{command.value.trimEnd()}</code>
              <span><strong>{command.label}</strong><small>{command.description}</small></span>
            </button>
          ))}
          <div className="command-menu-footer"><kbd>↑</kbd><kbd>↓</kbd> 选择 <kbd>Tab</kbd> 补全 <kbd>Esc</kbd> 关闭</div>
        </div>
      )}
      {fileToken && fileSuggestions.length > 0 && (
        <div className="command-menu" role="listbox" aria-label="文件建议">
          <div className="command-menu-header"><FileText size={14} /> 文件</div>
          {fileSuggestions.map((file, index) => (
            <button
              className={index === fileSelectedIndex ? "command-item selected" : "command-item"}
              key={file.relative_path}
              role="option"
              aria-selected={index === fileSelectedIndex}
              onMouseEnter={() => setFileSelectedIndex(index)}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => completeFile(index)}
              type="button"
            >
              <code>@{file.mention_path}</code>
              <span><strong>{file.display_path}</strong><small>{file.kind === "directory" ? "目录" : formatFileSize(file.size)}</small></span>
            </button>
          ))}
          <div className="command-menu-footer"><kbd>↑</kbd><kbd>↓</kbd> 选择 <kbd>Tab</kbd> 补全 <kbd>Esc</kbd> 关闭</div>
        </div>
      )}
      <ComposerPrimitive.Root className="composer">
        <ComposerPrimitive.Input
          ref={inputRef}
          placeholder="输入消息，或输入 / 查看命令..."
          rows={1}
          onKeyDown={handleKeyDown}
          onKeyUp={refreshFileToken}
          onClick={refreshFileToken}
        />
        <ComposerPrimitive.Send
          className="icon-button primary"
          aria-label="发送"
          onMouseDown={() => rememberInput(text)}
        ><Send size={18} /></ComposerPrimitive.Send>
        <ComposerPrimitive.Cancel className="icon-button danger" aria-label="停止"><Square size={16} /></ComposerPrimitive.Cancel>
      </ComposerPrimitive.Root>
    </div>
  );
}

function ChatThread({
  sessionId,
  todos,
  stats,
  onOpenContext,
}: {
  sessionId: string;
  todos: TodoItem[];
  stats?: DebugStats;
  onOpenContext: () => void;
}) {
  return (
    <ThreadPrimitive.Root className="thread">
      <ThreadPrimitive.Viewport className="viewport">
        <ThreadPrimitive.Empty>
          <div className="empty"><Bot size={30} /><h2>和 Remi 开始新的对话</h2><p>消息与会话保存在本机。</p></div>
        </ThreadPrimitive.Empty>
        <ThreadPrimitive.Messages components={{ UserMessage, AssistantMessage }} />
        <ThreadPrimitive.ViewportFooter className="composer-wrap">
          <div className="composer-status">
            <ActiveTodoPanel items={todos} />
            <DebugPanel stats={stats} onOpenContext={onOpenContext} />
          </div>
          <CommandComposer sessionId={sessionId} />
        </ThreadPrimitive.ViewportFooter>
      </ThreadPrimitive.Viewport>
    </ThreadPrimitive.Root>
  );
}

function ActiveTodoPanel({ items }: { items: TodoItem[] }) {
  const unfinished = items.filter((item) => !item.done);
  if (!unfinished.length) return null;
  const latestBatchId = unfinished
    .map((item) => item.batch_id)
    .filter((id): id is number => id !== undefined && id !== null)
    .at(-1);
  const activeItems = latestBatchId === undefined
    ? unfinished
    : items
        .filter((item) => item.batch_id === latestBatchId)
        .sort((a, b) => (a.batch_index ?? a.id) - (b.batch_index ?? b.id));
  const title = activeItems[0]?.batch_title ?? "Active Todo";
  const completed = activeItems.filter((item) => item.done).length;
  return (
    <details className="active-todos fixed-panel" open>
      <summary>
        <ListTodo size={15} />
        <span>{title}</span>
        <small>{completed}/{activeItems.length}</small>
      </summary>
      <div>
        {activeItems.map((item) => (
          <div className={item.done ? "todo-row done" : "todo-row"} key={item.id}>
            {item.done ? <CheckCircle2 size={15} /> : <Circle size={15} />}
            <span><strong>{item.content}</strong>{item.description && <small>{item.description}</small>}</span>
          </div>
        ))}
      </div>
    </details>
  );
}

const MODEL_INPUT_FILTERS: { value: "all" | ModelInputCategory; label: string }[] = [
  { value: "all", label: "All" },
  { value: "system_prompt", label: "System" },
  { value: "history", label: "History" },
  { value: "tool_input", label: "Tool input" },
  { value: "tool_output", label: "Tool output" },
  { value: "current_user", label: "Current" },
  { value: "metadata", label: "Metadata" },
  { value: "user_state", label: "State" },
];

function categoryLabel(category: ModelInputCategory) {
  return category.replaceAll("_", " ");
}

function shortRunId(runId: string) {
  return runId.length > 8 ? runId.slice(0, 8) : runId;
}

function ModelInputPanel({
  sessionId,
  onClose,
  onError,
}: {
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}) {
  const [items, setItems] = useState<ModelInputSummary[]>([]);
  const [selectedRunId, setSelectedRunId] = useState<string>();
  const [snapshot, setSnapshot] = useState<ModelInputSnapshot>();
  const [filter, setFilter] = useState<"all" | ModelInputCategory>("all");
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    api.modelInputs(sessionId)
      .then((next) => {
        if (cancelled) return;
        setItems(next);
        setSelectedRunId(next[0]?.run_id);
      })
      .catch((reason) => onError(String(reason)))
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [onError, sessionId]);

  useEffect(() => {
    if (!selectedRunId) {
      setSnapshot(undefined);
      return;
    }
    let cancelled = false;
    api.modelInput(sessionId, selectedRunId)
      .then((next) => {
        if (!cancelled) setSnapshot(next);
      })
      .catch((reason) => onError(String(reason)));
    return () => {
      cancelled = true;
    };
  }, [onError, selectedRunId, sessionId]);

  const visibleSegments = snapshot?.segments.filter((segment) =>
    filter === "all" ? true : segment.category === filter,
  ) ?? [];
  const actual = snapshot?.totals.prompt_tokens;
  const contextPercent = snapshot?.totals.context_tokens
    ? Math.min(100, ((snapshot.totals.max_prompt_tokens ?? actual ?? 0) / snapshot.totals.context_tokens) * 100)
    : undefined;

  return (
    <div className="modal-backdrop" role="dialog" aria-modal="true">
      <section className="model-input-panel">
        <header>
          <div>
            <strong>Model Context</strong>
            <span>{snapshot ? `${snapshot.model_profile_id} · ${snapshot.model}` : "captured run input"}</span>
          </div>
          <button className="menu" onClick={onClose} aria-label="关闭">×</button>
        </header>
        {loading ? (
          <div className="model-input-empty">加载中...</div>
        ) : !items.length ? (
          <div className="model-input-empty">No captured model input for this session.</div>
        ) : (
          <>
            <div className="model-input-toolbar">
              <select value={selectedRunId} onChange={(event) => setSelectedRunId(event.target.value)}>
                {items.map((item) => (
                  <option key={item.run_id} value={item.run_id}>
                    {shortRunId(item.run_id)} · {new Date(item.created_at).toLocaleString()}
                  </option>
                ))}
              </select>
              <div className="model-input-tabs">
                {MODEL_INPUT_FILTERS.map((item) => (
                  <button
                    key={item.value}
                    className={filter === item.value ? "active" : ""}
                    onClick={() => setFilter(item.value)}
                    type="button"
                  >
                    {item.label}
                  </button>
                ))}
              </div>
            </div>
            {snapshot && (
              <div className="model-input-summary">
                <div><span>Estimated input</span><strong>{formatTokens(snapshot.totals.estimated_tokens)}</strong></div>
                <div><span>Actual input</span><strong>{actual === undefined || actual === null ? "N/A" : formatTokens(actual)}</strong></div>
                <div><span>Output</span><strong>{formatTokens(snapshot.totals.completion_tokens ?? 0)}</strong></div>
                <div><span>Context</span><strong>{contextPercent === undefined ? "N/A" : `${contextPercent.toFixed(1)}%`}</strong></div>
              </div>
            )}
            <div className="model-input-segments">
              {visibleSegments.map((segment) => (
                <details className={`model-input-segment ${segment.category}`} key={segment.id} open={segment.category !== "history"}>
                  <summary>
                    <span>{categoryLabel(segment.category)}</span>
                    <strong>{segment.title}</strong>
                    {segment.role && <code>{segment.role}</code>}
                    <small>{formatTokens(segment.token_estimate)} est.</small>
                  </summary>
                  <pre>{segment.content}</pre>
                </details>
              ))}
              {!visibleSegments.length && <div className="model-input-empty">No segments in this category.</div>}
            </div>
          </>
        )}
      </section>
    </div>
  );
}

function SecretsPanel({
  secrets,
  onClose,
  onRefresh,
  onSave,
  onDelete,
}: {
  secrets?: SecretList;
  onClose: () => void;
  onRefresh: () => void;
  onSave: (key: string, value: string) => Promise<void>;
  onDelete: (key: string) => Promise<void>;
}) {
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const [saving, setSaving] = useState(false);
  const sortedEntries = [...(secrets?.entries ?? [])].sort((a, b) => a.key.localeCompare(b.key));

  const submit = async () => {
    if (!key.trim() || !value) return;
    setSaving(true);
    try {
      await onSave(key.trim(), value);
      setKey("");
      setValue("");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="modal-backdrop" role="dialog" aria-modal="true">
      <section className="secrets-panel">
        <header>
          <div><strong>Secrets</strong><span>{secrets?.backend ?? "loading..."}</span></div>
          <button className="menu" onClick={onClose} aria-label="关闭">×</button>
        </header>
        <div className="secret-list">
          {sortedEntries.length ? sortedEntries.map((entry) => (
            <div className="secret-row" key={entry.key}>
              <code>{entry.key}</code>
              <span>已设置</span>
              <button onClick={() => void onDelete(entry.key)}>删除</button>
            </div>
          )) : <p className="muted">No secrets stored.</p>}
        </div>
        <div className="secret-form">
          <label>Key<input value={key} onChange={(event) => setKey(event.target.value)} placeholder="EXA_API_KEY" /></label>
          <label>Value<input value={value} onChange={(event) => setValue(event.target.value)} type="password" placeholder="secret value" /></label>
          <div className="secret-actions">
            <button onClick={onRefresh}>刷新</button>
            <button className="primary-action" disabled={saving || !key.trim() || !value} onClick={() => void submit()}>
              {saving ? "保存中..." : "保存 / 更新"}
            </button>
          </div>
        </div>
      </section>
    </div>
  );
}

export default function App() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [activeId, setActiveId] = useState<string>();
  const [history, setHistory] = useState<HistoryMessage[]>([]);
  const [activeRun, setActiveRun] = useState<ActiveRun | null>(null);
  const [loading, setLoading] = useState(true);
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [error, setError] = useState<string>();
  const [todos, setTodos] = useState<TodoItem[]>([]);
  const [stats, setStats] = useState<DebugStats>();
  const [secrets, setSecrets] = useState<SecretList>();
  const [secretsOpen, setSecretsOpen] = useState(false);
  const [modelInputOpen, setModelInputOpen] = useState(false);

  const refresh = useCallback(async () => {
    const next = await api.listSessions();
    setSessions(next);
    return next;
  }, []);

  useEffect(() => {
    refresh()
      .then(async (items) => {
        const selected = items[0] ?? (await api.createSession());
        if (!items.length) setSessions([selected]);
        setActiveId(selected.id);
      })
      .catch((reason) => setError(String(reason)))
      .finally(() => setLoading(false));
  }, [refresh]);

  useEffect(() => {
    if (!activeId) return;
    setStats(undefined);
    setActiveRun(null);
    setLoading(true);
    Promise.all([api.history(activeId), api.todos(activeId), api.activeRun(activeId)])
      .then(([messages, items, run]) => {
        setHistory(messages);
        setTodos(items);
        setActiveRun(run);
      })
      .catch((reason) => setError(String(reason)))
      .finally(() => setLoading(false));
  }, [activeId]);

  const refreshTodos = useCallback(() => {
    if (!activeId) return Promise.resolve();
    return api.todos(activeId).then(setTodos);
  }, [activeId]);

  const refreshSecrets = useCallback(() => {
    return api.listSecrets().then(setSecrets);
  }, []);

  const createSession = async () => {
    const session = await api.createSession();
    setSessions((current) => [session, ...current]);
    setActiveId(session.id);
  };

  const renameSession = async (session: Session) => {
    const title = window.prompt("会话名称", session.title ?? "新对话");
    if (title === null) return;
    const updated = await api.renameSession(session.id, title);
    setSessions((current) => current.map((item) => item.id === updated.id ? updated : item));
  };

  const forkSession = async (session: Session) => {
    if (session.active_run) {
      setError("当前会话正在运行，结束或取消后再 fork。");
      return;
    }
    try {
      const fork = await api.forkSession(session.id);
      setSessions((current) => [fork, ...current]);
      setActiveId(fork.id);
    } catch (reason) {
      setError(String(reason));
    }
  };

  const handleSessionForked = (session: Session) => {
    setSessions((current) => [
      session,
      ...current.filter((item) => item.id !== session.id),
    ]);
    setActiveId(session.id);
  };

  const deleteSession = async (session: Session) => {
    if (!window.confirm(`删除“${session.title ?? "新对话"}”及全部历史？`)) return;
    await api.deleteSession(session.id);
    const next = sessions.filter((item) => item.id !== session.id);
    if (next.length) {
      setSessions(next);
      if (activeId === session.id) setActiveId(next[0].id);
    } else {
      await createSession();
    }
  };

  const openSecrets = async () => {
    setSecretsOpen(true);
    try {
      await refreshSecrets();
    } catch (reason) {
      setError(String(reason));
    }
  };

  const saveSecret = async (key: string, value: string) => {
    try {
      setSecrets(await api.setSecret(key, value));
    } catch (reason) {
      setError(String(reason));
      throw reason;
    }
  };

  const deleteSecret = async (key: string) => {
    if (!window.confirm(`Delete secret "${key}"?`)) return;
    try {
      setSecrets(await api.deleteSecret(key));
    } catch (reason) {
      setError(String(reason));
    }
  };

  const active = sessions.find((session) => session.id === activeId);
  return (
    <div className="app-shell">
      <aside className={sidebarOpen ? "sidebar open" : "sidebar"}>
        <div className="brand"><div className="brand-mark">R</div><div><strong>Remi</strong><span>Web Chat</span></div></div>
        <button className="new-chat" onClick={createSession}><Plus size={17} /> 新对话</button>
        <button className="new-chat secondary" onClick={() => void openSecrets()}><KeyRound size={17} /> Secrets</button>
        <div className="session-list">
          {sessions.map((session) => (
            <div className={[
              "session",
              session.id === activeId ? "active" : "",
              session.active_run ? "running" : "",
            ].filter(Boolean).join(" ")} key={session.id}>
              <button className="session-title" onClick={() => setActiveId(session.id)}>
                <span>{session.title || "新对话"}</span>
                {session.active_run && <small>运行中</small>}
              </button>
              <button
                onClick={() => forkSession(session)}
                aria-label="Fork"
                disabled={Boolean(session.active_run)}
                title={session.active_run ? "运行中，结束或取消后再 fork" : "Fork 会话"}
              >
                <GitBranch size={14} />
              </button>
              <button onClick={() => renameSession(session)} aria-label="重命名"><Pencil size={14} /></button>
              <button onClick={() => deleteSession(session)} aria-label="删除"><Trash2 size={14} /></button>
            </div>
          ))}
        </div>
      </aside>
      {secretsOpen && (
        <SecretsPanel
          secrets={secrets}
          onClose={() => setSecretsOpen(false)}
          onRefresh={() => void refreshSecrets().catch((reason) => setError(String(reason)))}
          onSave={saveSecret}
          onDelete={deleteSecret}
        />
      )}
      {modelInputOpen && activeId && (
        <ModelInputPanel
          sessionId={activeId}
          onClose={() => setModelInputOpen(false)}
          onError={setError}
        />
      )}
      <main>
        <header>
          <button className="menu" onClick={() => setSidebarOpen((value) => !value)}><Menu size={20} /></button>
          <div><strong>{active?.title || "新对话"}</strong><span>本机单用户</span></div>
          <button className="header-action" onClick={() => setModelInputOpen(true)} title="查看模型输入上下文">
            <FileText size={16} />
            <span>Context</span>
          </button>
        </header>
        {error && <div className="error" onClick={() => setError(undefined)}>{error}</div>}
        {loading || !activeId ? <div className="loading">加载中...</div> : (
          <RemiRuntimeProvider
            key={activeId}
            sessionId={activeId}
            history={history}
            activeRun={activeRun}
            onTodosChanged={setTodos}
            onStatsChanged={setStats}
            onSessionForked={handleSessionForked}
            onStarted={() => {
              void refresh();
            }}
            onFinished={() => {
              void refresh();
              void refreshTodos();
            }}
          >
            <ChatThread
              sessionId={activeId}
              todos={todos}
              stats={stats}
              onOpenContext={() => setModelInputOpen(true)}
            />
          </RemiRuntimeProvider>
        )}
      </main>
    </div>
  );
}
