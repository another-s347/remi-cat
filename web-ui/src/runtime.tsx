import {
  AssistantRuntimeProvider,
  type AppendMessage,
  type ThreadAssistantMessagePart,
  type ThreadMessageLike,
  useExternalStoreRuntime,
} from "@assistant-ui/react";
import { useCallback, useEffect, useMemo, useRef, useState, type PropsWithChildren } from "react";
import {
  api,
  streamRun,
  type ActiveRun,
  type ContextCompactionEvent,
  type DebugStats,
  type HistoryMessage,
  type PrettyToolCall,
  type Session,
  type ToolApprovalDecision,
  type ToolApprovalRequest,
  type TodoItem,
  type UserQuestionRequest,
  type UserQuestionResponse,
} from "./api";

const TOOL_TICK_MS = 500;

function contentText(content: AppendMessage["content"]) {
  return content
    .filter((part): part is { type: "text"; text: string } => part.type === "text")
    .map((part) => part.text)
    .join("");
}

type PendingSteer = {
  messageId: string;
  text: string;
};

type ActiveRunState = {
  id: string;
  controller: AbortController;
  ready: boolean;
  pendingSteers: PendingSteer[];
  flushing: boolean;
};

export function toInitialMessages(history: HistoryMessage[]): ThreadMessageLike[] {
  const messages: ThreadMessageLike[] = [];
  const toolIndexes = new Map<string, { messageIndex: number; partIndex: number }>();

  for (const message of history) {
    if (message.role === "user") {
      messages.push({
        id: message.id,
        role: "user",
        content: [{ type: "text", text: message.text }],
        createdAt: message.timestamp ? new Date(message.timestamp) : undefined,
      });
      continue;
    }

    if (message.role === "assistant") {
      const content: ThreadAssistantMessagePart[] = [];
      if (message.text) content.push({ type: "text", text: message.text });
      for (const call of message.tool_calls ?? []) {
        if (call.function.name.startsWith("todo__")) continue;
        const args = parseToolArguments(call.function.arguments);
        toolIndexes.set(call.id, {
          messageIndex: messages.length,
          partIndex: content.length,
        });
        content.push({
          type: "tool-call",
          toolCallId: call.id,
          toolName: call.function.name,
          args: { raw: args } as never,
          argsText: JSON.stringify(args, null, 2),
        });
      }
      messages.push({
        id: message.id,
        role: "assistant",
        content,
        createdAt: message.timestamp ? new Date(message.timestamp) : undefined,
        status: { type: "complete", reason: "stop" },
      });
      continue;
    }

    if (message.role === "tool" && message.tool_call_id) {
      const index = toolIndexes.get(message.tool_call_id);
      if (!index) continue;
      const assistant = messages[index.messageIndex];
      if (assistant.role !== "assistant") continue;
      const content = [...assistant.content] as ThreadAssistantMessagePart[];
      const part = content[index.partIndex];
      if (part?.type !== "tool-call") continue;
      content[index.partIndex] = {
        ...part,
        args: {
          ...(part.args as Record<string, unknown>),
          pretty: message.pretty ?? undefined,
        } as never,
        argsText: message.pretty ? "" : part.argsText,
        result: part.toolName === "__remi_supervisor"
          ? parseToolArguments(message.text)
          : {
              raw: message.text,
              pretty: message.pretty ?? undefined,
            },
      };
      messages[index.messageIndex] = { ...assistant, content };
    }
  }

  return messages;
}

function parseToolArguments(value: string) {
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
}

function approvalToolCallId(request: ToolApprovalRequest) {
  return `approval-${request.id}`;
}

function userQuestionToolCallId(request: UserQuestionRequest) {
  return `user-question-${request.id}`;
}

function upsertApprovalPart(
  parts: ThreadAssistantMessagePart[],
  indexes: Map<string, number>,
  request: ToolApprovalRequest,
  decision?: ToolApprovalDecision,
) {
  const toolCallId = approvalToolCallId(request);
  const payload = { request, decision };
  const index = indexes.get(toolCallId);
  if (index !== undefined) {
    const part = parts[index];
    if (part?.type === "tool-call") {
      parts[index] = {
        ...part,
        args: payload as never,
        argsText: "",
        result: decision ? payload : undefined,
      };
    }
    return;
  }
  indexes.set(toolCallId, parts.length);
  parts.push({
    type: "tool-call",
    toolCallId,
    toolName: "__remi_approval",
    args: payload as never,
    argsText: "",
    result: decision ? payload : undefined,
  });
}

function upsertUserQuestionPart(
  parts: ThreadAssistantMessagePart[],
  indexes: Map<string, number>,
  request: UserQuestionRequest,
  response?: UserQuestionResponse,
) {
  const toolCallId = userQuestionToolCallId(request);
  const payload = { request, response };
  const index = indexes.get(toolCallId);
  if (index !== undefined) {
    const part = parts[index];
    if (part?.type === "tool-call") {
      parts[index] = {
        ...part,
        args: payload as never,
        argsText: "",
        result: response ? payload : undefined,
      };
    }
    return;
  }
  indexes.set(toolCallId, parts.length);
  parts.push({
    type: "tool-call",
    toolCallId,
    toolName: "__remi_user_question",
    args: payload as never,
    argsText: "",
    result: response ? payload : undefined,
  });
}

function contextCompactionToolCallId(event: ContextCompactionEvent) {
  return `context-compaction-${event.id}`;
}

function upsertContextCompactionPart(
  parts: ThreadAssistantMessagePart[],
  indexes: Map<string, number>,
  event: ContextCompactionEvent,
) {
  const toolCallId = contextCompactionToolCallId(event);
  const index = indexes.get(toolCallId);
  if (index !== undefined) {
    const part = parts[index];
    if (part?.type === "tool-call") {
      parts[index] = {
        ...part,
        args: event as never,
        argsText: "",
        result: event.status === "started" ? undefined : event,
      };
    }
    return;
  }
  indexes.set(toolCallId, parts.length);
  parts.push({
    type: "tool-call",
    toolCallId,
    toolName: "__remi_context_compaction",
    args: event as never,
    argsText: "",
    result: event.status === "started" ? undefined : event,
  });
}

type ProviderProps = PropsWithChildren<{
  sessionId: string;
  history: HistoryMessage[];
  activeRun?: ActiveRun | null;
  onFinished: () => void;
  onStarted: () => void;
  onTodosChanged: (items: TodoItem[]) => void;
  onStatsChanged: (stats: DebugStats) => void;
  onSessionForked: (session: Session) => void;
}>;

function appendSupervisorProgress(
  parts: ThreadAssistantMessagePart[],
  supervisorIndex: number | undefined,
  runId: string,
  data: Record<string, unknown>,
) {
  let index = supervisorIndex;
  if (index === undefined) {
    index = parts.length;
    parts.push({
      type: "tool-call",
      toolCallId: `${runId}-supervisor`,
      toolName: "__remi_supervisor",
      args: { events: [] },
      argsText: "",
    });
  }

  const part = parts[index];
  if (part?.type !== "tool-call") return index;

  const args = part.args as { events?: Record<string, unknown>[] };
  const events = [...(args.events ?? [])];
  const eventType = String(data.type ?? "");
  if (eventType === "output_delta") {
    const text = String(data.content ?? "");
    const last = events.at(-1);
    if (last && String(last.type ?? "") === "output_delta") {
      events[events.length - 1] = {
        ...last,
        content: String(last.content ?? "") + text,
      };
    } else {
      events.push({ ...data, content: text });
    }
  } else {
    events.push(data);
  }

  parts[index] = {
    ...part,
    args: { events } as never,
  };
  return index;
}

function applySupervisorReport(
  parts: ThreadAssistantMessagePart[],
  supervisorIndex: number | undefined,
  runId: string,
  data: Record<string, unknown>,
) {
  let index = supervisorIndex;
  if (index === undefined) {
    index = parts.length;
    parts.push({
      type: "tool-call",
      toolCallId: `${runId}-supervisor`,
      toolName: "__remi_supervisor",
      args: { events: [] },
      argsText: "",
      result: data,
    });
    return index;
  }
  const part = parts[index];
  if (part?.type === "tool-call") {
    parts[index] = { ...part, result: data };
  }
  return index;
}

function subSessionKey(data: Record<string, unknown>, fallback: string) {
  const threadId = String(data.sub_thread_id ?? data.thread_id ?? "");
  const runId = String(data.sub_run_id ?? data.run_id ?? "");
  return threadId || runId ? `${threadId}:${runId}` : fallback;
}

function appendSubSessionEvent(
  parts: ThreadAssistantMessagePart[],
  indexes: Map<string, number>,
  runId: string,
  sequence: number,
  data: Record<string, unknown>,
) {
  const subType = String(data.sub_type ?? "");
  if (subType === "delta") {
    return;
  }
  const displayData =
    subType === "thinking_end"
      ? { ...data, content: undefined }
      : subType === "done"
        ? { ...data, final_output: undefined }
        : data;
  const key = subSessionKey(data, `${runId}-sub-session-${sequence}`);
  const toolCallId = `${runId}-sub-session-${key}`;
  let index = indexes.get(toolCallId);
  if (index === undefined) {
    index = parts.length;
    indexes.set(toolCallId, index);
    parts.push({
      type: "tool-call",
      toolCallId,
      toolName: "__remi_sub_session",
      args: {
        agentName: data.agent_name,
        title: data.title,
        threadId: data.sub_thread_id ?? data.thread_id,
        runId: data.sub_run_id ?? data.run_id,
        events: [],
      } as never,
      argsText: "",
    });
  }

  const part = parts[index];
  if (part?.type !== "tool-call") return;
  const args = part.args as {
    agentName?: unknown;
    title?: unknown;
    threadId?: unknown;
    runId?: unknown;
    events?: Record<string, unknown>[];
  };
  const events = [...(args.events ?? [])];
  events.push(displayData);

  parts[index] = {
    ...part,
    args: {
      agentName: displayData.agent_name ?? args.agentName,
      title: displayData.title ?? args.title,
      threadId: displayData.sub_thread_id ?? displayData.thread_id ?? args.threadId,
      runId: displayData.sub_run_id ?? displayData.run_id ?? args.runId,
      events,
    } as never,
    result: subType === "done" || subType === "error" ? displayData : part.result,
  };
}

export function RemiRuntimeProvider({
  sessionId,
  history,
  activeRun,
  onFinished,
  onStarted,
  onTodosChanged,
  onStatsChanged,
  onSessionForked,
  children,
}: ProviderProps) {
  const [messages, setMessages] = useState<ThreadMessageLike[]>(() =>
    toInitialMessages(history),
  );
  const [isRunning, setIsRunning] = useState(false);
  const activeRunRef = useRef<ActiveRunState | undefined>(undefined);
  const resumedRunId = useRef<string | undefined>(undefined);
  const onFinishedRef = useRef(onFinished);
  const onStartedRef = useRef(onStarted);
  const onTodosChangedRef = useRef(onTodosChanged);
  const onStatsChangedRef = useRef(onStatsChanged);
  const onSessionForkedRef = useRef(onSessionForked);

  useEffect(() => {
    onFinishedRef.current = onFinished;
    onStartedRef.current = onStarted;
    onTodosChangedRef.current = onTodosChanged;
    onStatsChangedRef.current = onStatsChanged;
    onSessionForkedRef.current = onSessionForked;
  }, [onFinished, onSessionForked, onStarted, onStatsChanged, onTodosChanged]);

  const completeSteerMessage = useCallback((messageId: string) => {
    setMessages((current) =>
      current.map((item) =>
        item.id === messageId
          ? { ...item, status: { type: "complete", reason: "stop" } }
          : item,
      ),
    );
  }, []);

  const failSteerMessage = useCallback((messageId: string, error: unknown) => {
    setMessages((current) =>
      current.map((item) =>
        item.id === messageId
          ? {
              ...item,
              status: {
                type: "incomplete",
                reason: "error",
                error: String(error),
              },
            }
          : item,
      ),
    );
  }, []);

  const failPendingSteers = useCallback((run: ActiveRunState, error: unknown) => {
    const pending = run.pendingSteers.splice(0);
    if (!pending.length) return;
    const pendingIds = new Set(pending.map((item) => item.messageId));
    setMessages((current) =>
      current.map((item) =>
        item.id !== undefined && pendingIds.has(item.id)
          ? {
              ...item,
              status: {
                type: "incomplete",
                reason: "error",
                error: String(error),
              },
            }
          : item,
      ),
    );
  }, []);

  const sendSteer = useCallback(
    async (runId: string, messageId: string, text: string) => {
      try {
        await api.steerRun(sessionId, runId, text);
        completeSteerMessage(messageId);
      } catch (error) {
        failSteerMessage(messageId, error);
        throw error;
      }
    },
    [completeSteerMessage, failSteerMessage, sessionId],
  );

  const flushPendingSteers = useCallback(
    (run: ActiveRunState) => {
      if (run.flushing) return;
      run.flushing = true;
      void (async () => {
        try {
          while (
            activeRunRef.current === run &&
            run.ready &&
            run.pendingSteers.length > 0
          ) {
            const pending = run.pendingSteers.shift();
            if (!pending) continue;
            await sendSteer(run.id, pending.messageId, pending.text).catch(() => undefined);
          }
        } finally {
          run.flushing = false;
        }
      })();
    },
    [sendSteer],
  );

  useEffect(() => {
    if (!activeRun || resumedRunId.current === activeRun.run_id) return;
    resumedRunId.current = activeRun.run_id;
    const runId = activeRun.run_id;
    const assistantId = crypto.randomUUID();
    const controller = new AbortController();
    const runState: ActiveRunState = {
      id: runId,
      controller,
      ready: false,
      pendingSteers: [],
      flushing: false,
    };
    activeRunRef.current = runState;
    setIsRunning(true);
    onStartedRef.current();
    setMessages((current) => [
      ...current,
      {
        id: crypto.randomUUID(),
        role: "user",
        content: [{ type: "text", text: activeRun.text }],
        createdAt: new Date(),
      },
      {
        id: assistantId,
        role: "assistant",
        content: [],
        createdAt: new Date(),
        status: { type: "running" },
      },
    ]);

    const parts: ThreadAssistantMessagePart[] = [];
    const toolIndexes = new Map<string, number>();
    const runningToolIds = new Set<string>();
    let ticker: number | undefined;
    let supervisorIndex: number | undefined;
    const subSessionIndexes = new Map<string, number>();
    const updateAssistant = (status: ThreadMessageLike["status"]) => {
      setMessages((current) =>
        current.map((item) =>
          item.id === assistantId
            ? { ...item, content: [...parts], status }
            : item,
        ),
      );
    };
    const stopTicker = () => {
      if (ticker !== undefined) {
        window.clearInterval(ticker);
        ticker = undefined;
      }
    };
    const startTicker = () => {
      if (ticker !== undefined) return;
      ticker = window.setInterval(() => {
        if (!runningToolIds.size) {
          stopTicker();
          return;
        }
        updateAssistant({ type: "running" });
      }, TOOL_TICK_MS);
    };

    void (async () => {
      try {
        for await (const event of streamRun(
          sessionId,
          runId,
          activeRun.text,
          controller.signal,
          () => {
            runState.ready = true;
            flushPendingSteers(runState);
          },
        )) {
          const data = event.data ?? {};
          switch (event.event) {
            case "text_delta": {
              const text = String(data.text ?? "");
              const last = parts.at(-1);
              if (last?.type === "text") {
                parts[parts.length - 1] = { ...last, text: last.text + text };
              } else {
                parts.push({ type: "text", text });
              }
              break;
            }
            case "thinking_delta": {
              const text = String(data.text ?? "");
              const last = parts.at(-1);
              if (last?.type === "reasoning") {
                parts[parts.length - 1] = { ...last, text: last.text + text };
              } else {
                parts.push({ type: "reasoning", text });
              }
              break;
            }
            case "tool_started": {
              const toolName = String(data.tool_name ?? "tool");
              if (toolName.startsWith("todo__")) break;
              const toolCallId = String(data.call_id ?? `${runId}-${event.sequence}`);
              const pretty = data.pretty as PrettyToolCall | undefined;
              const args = (data.args ?? {}) as Extract<
                ThreadAssistantMessagePart,
                { type: "tool-call" }
              >["args"];
              const existingIndex = toolIndexes.get(toolCallId);
              if (existingIndex !== undefined) {
                const part = parts[existingIndex];
                if (part?.type === "tool-call") {
                  const existingPretty = (part.args as { pretty?: PrettyToolCall }).pretty;
                  const runningPretty = pretty
                    ? { ...pretty, started_at_ms: existingPretty?.started_at_ms ?? performance.now() }
                    : existingPretty;
                  parts[existingIndex] = {
                    ...part,
                    toolName,
                    args: { raw: args, pretty: runningPretty } as never,
                    argsText: pretty ? "" : JSON.stringify(args, null, 2),
                  };
                }
                break;
              }
              const runningPretty = pretty
                ? { ...pretty, started_at_ms: performance.now() }
                : undefined;
              toolIndexes.set(toolCallId, parts.length);
              runningToolIds.add(toolCallId);
              startTicker();
              parts.push({
                type: "tool-call",
                toolCallId,
                toolName,
                args: { raw: args, pretty: runningPretty } as never,
                argsText: pretty ? "" : JSON.stringify(args, null, 2),
              });
              break;
            }
            case "tool_completed": {
              const toolName = String(data.tool_name ?? "tool");
              if (toolName.startsWith("todo__")) break;
              const toolCallId = String(data.call_id ?? `${runId}-${event.sequence}`);
              runningToolIds.delete(toolCallId);
              const index = toolIndexes.get(toolCallId);
              if (index === undefined) break;
              const part = parts[index];
              if (part?.type === "tool-call") {
                parts[index] = {
                  ...part,
                  result: {
                    raw: data.result,
                    pretty: data.pretty as PrettyToolCall | undefined,
                  },
                };
              }
              break;
            }
            case "approval_requested":
            case "approval_updated": {
              upsertApprovalPart(
                parts,
                toolIndexes,
                data as unknown as ToolApprovalRequest,
              );
              break;
            }
            case "approval_resolved": {
              upsertApprovalPart(
                parts,
                toolIndexes,
                data.request as ToolApprovalRequest,
                data.decision as ToolApprovalDecision,
              );
              break;
            }
            case "user_question_requested":
            case "user_question_updated": {
              upsertUserQuestionPart(
                parts,
                toolIndexes,
                data as unknown as UserQuestionRequest,
              );
              break;
            }
            case "user_question_resolved": {
              upsertUserQuestionPart(
                parts,
                toolIndexes,
                data.request as UserQuestionRequest,
                data.response as UserQuestionResponse,
              );
              break;
            }
            case "supervisor_progress": {
              supervisorIndex = appendSupervisorProgress(parts, supervisorIndex, runId, data);
              break;
            }
            case "supervisor_report": {
              supervisorIndex = applySupervisorReport(parts, supervisorIndex, runId, data);
              break;
            }
            case "sub_session": {
              appendSubSessionEvent(parts, subSessionIndexes, runId, event.sequence, data);
              break;
            }
            case "context_compaction": {
              upsertContextCompactionPart(
                parts,
                toolIndexes,
                data as unknown as ContextCompactionEvent,
              );
              break;
            }
            case "steer_injected": {
              const count = Number(data.count ?? 1);
              const preview = String(data.preview ?? "");
              parts.push({
                type: "text",
                text: `\n\n[Steer injected: ${count}${preview ? ` - ${preview}` : ""}]\n\n`,
              });
              break;
            }
            case "stats":
              onStatsChangedRef.current(data as DebugStats);
              break;
            case "todo_state":
              onTodosChangedRef.current((data.items ?? []) as TodoItem[]);
              break;
            case "session_forked":
              onSessionForkedRef.current(data as Session);
              break;
            case "error":
              throw new Error(String(data.message ?? "run failed"));
          }
          updateAssistant({ type: "running" });
        }
        updateAssistant({ type: "complete", reason: "stop" });
      } catch (error) {
        if (controller.signal.aborted) {
          updateAssistant({ type: "incomplete", reason: "cancelled" });
        } else {
          updateAssistant({
            type: "incomplete",
            reason: "error",
            error: String(error),
          });
        }
      } finally {
        runningToolIds.clear();
        stopTicker();
        if (activeRunRef.current?.id === runId) {
          failPendingSteers(
            activeRunRef.current,
            controller.signal.aborted
              ? "run cancelled before steer could be sent"
              : "run finished before steer could be sent",
          );
          activeRunRef.current = undefined;
        }
        setIsRunning(false);
        onFinishedRef.current();
      }
    })();

    return () => {
      controller.abort();
      stopTicker();
    };
  }, [activeRun, failPendingSteers, flushPendingSteers, sessionId]);

  const onNew = useCallback(
    async (message: AppendMessage) => {
      const activeRun = activeRunRef.current;
      if (activeRun) {
        const text = contentText(message.content);
        const steerId = crypto.randomUUID();
        setMessages((current) => [
          ...current,
          {
            id: steerId,
            role: "user",
            content: message.content,
            createdAt: new Date(),
            status: { type: "running" },
          },
        ]);
        void api.appendInputHistory(sessionId, text).catch(() => undefined);
        if (!activeRun.ready || activeRun.flushing || activeRun.pendingSteers.length > 0) {
          activeRun.pendingSteers.push({ messageId: steerId, text });
          if (activeRun.ready) flushPendingSteers(activeRun);
          return;
        }
        await sendSteer(activeRun.id, steerId, text);
        return;
      }
      const runId = crypto.randomUUID();
      const assistantId = crypto.randomUUID();
      const controller = new AbortController();
      const runState: ActiveRunState = {
        id: runId,
        controller,
        ready: false,
        pendingSteers: [],
        flushing: false,
      };
      activeRunRef.current = runState;
      setIsRunning(true);
      onStartedRef.current();
      setMessages((current) => [
        ...current,
        {
          id: crypto.randomUUID(),
          role: "user",
          content: message.content,
          createdAt: new Date(),
        },
        {
          id: assistantId,
          role: "assistant",
          content: [],
          createdAt: new Date(),
          status: { type: "running" },
        },
      ]);

      const parts: ThreadAssistantMessagePart[] = [];
      const toolIndexes = new Map<string, number>();
      const runningToolIds = new Set<string>();
      let ticker: number | undefined;
      let supervisorIndex: number | undefined;
      const subSessionIndexes = new Map<string, number>();
      const updateAssistant = (status: ThreadMessageLike["status"]) => {
        setMessages((current) =>
          current.map((item) =>
            item.id === assistantId
              ? { ...item, content: [...parts], status }
              : item,
          ),
        );
      };
      const stopTicker = () => {
        if (ticker !== undefined) {
          window.clearInterval(ticker);
          ticker = undefined;
        }
      };
      const startTicker = () => {
        if (ticker !== undefined) return;
        ticker = window.setInterval(() => {
          if (!runningToolIds.size) {
            stopTicker();
            return;
          }
          updateAssistant({ type: "running" });
        }, TOOL_TICK_MS);
      };

      try {
        void api.appendInputHistory(sessionId, contentText(message.content)).catch(() => undefined);
        for await (const event of streamRun(
          sessionId,
          runId,
          contentText(message.content),
          controller.signal,
          () => {
            runState.ready = true;
            flushPendingSteers(runState);
          },
        )) {
          const data = event.data ?? {};
          switch (event.event) {
            case "text_delta": {
              const text = String(data.text ?? "");
              const last = parts.at(-1);
              if (last?.type === "text") {
                parts[parts.length - 1] = { ...last, text: last.text + text };
              } else {
                parts.push({ type: "text", text });
              }
              break;
            }
            case "thinking_delta": {
              const text = String(data.text ?? "");
              const last = parts.at(-1);
              if (last?.type === "reasoning") {
                parts[parts.length - 1] = { ...last, text: last.text + text };
              } else {
                parts.push({ type: "reasoning", text });
              }
              break;
            }
            case "tool_started": {
              const toolName = String(data.tool_name ?? "tool");
              if (toolName.startsWith("todo__")) break;
              const toolCallId = String(data.call_id ?? `${runId}-${event.sequence}`);
              const pretty = data.pretty as PrettyToolCall | undefined;
              const args = (data.args ?? {}) as Extract<
                ThreadAssistantMessagePart,
                { type: "tool-call" }
              >["args"];
              const existingIndex = toolIndexes.get(toolCallId);
              if (existingIndex !== undefined) {
                const part = parts[existingIndex];
                if (part?.type === "tool-call") {
                  const existingPretty = (part.args as { pretty?: PrettyToolCall }).pretty;
                  const runningPretty = pretty
                    ? { ...pretty, started_at_ms: existingPretty?.started_at_ms ?? performance.now() }
                    : existingPretty;
                  parts[existingIndex] = {
                    ...part,
                    toolName,
                    args: { raw: args, pretty: runningPretty } as never,
                    argsText: pretty ? "" : JSON.stringify(args, null, 2),
                  };
                }
                break;
              }
              const runningPretty = pretty
                ? { ...pretty, started_at_ms: performance.now() }
                : undefined;
              toolIndexes.set(toolCallId, parts.length);
              runningToolIds.add(toolCallId);
              startTicker();
              parts.push({
                type: "tool-call",
                toolCallId,
                toolName,
                args: { raw: args, pretty: runningPretty } as never,
                argsText: pretty ? "" : JSON.stringify(args, null, 2),
              });
              break;
            }
            case "tool_completed": {
              const toolName = String(data.tool_name ?? "tool");
              if (toolName.startsWith("todo__")) break;
              const toolCallId = String(data.call_id ?? `${runId}-${event.sequence}`);
              runningToolIds.delete(toolCallId);
              const index = toolIndexes.get(toolCallId);
              if (index === undefined) break;
              const part = parts[index];
              if (part?.type === "tool-call") {
                parts[index] = {
                  ...part,
                  result: {
                    raw: data.result,
                    pretty: data.pretty as PrettyToolCall | undefined,
                  },
                };
              }
              break;
            }
            case "approval_requested":
            case "approval_updated": {
              upsertApprovalPart(
                parts,
                toolIndexes,
                data as unknown as ToolApprovalRequest,
              );
              break;
            }
            case "approval_resolved": {
              upsertApprovalPart(
                parts,
                toolIndexes,
                data.request as ToolApprovalRequest,
                data.decision as ToolApprovalDecision,
              );
              break;
            }
            case "user_question_requested":
            case "user_question_updated": {
              upsertUserQuestionPart(
                parts,
                toolIndexes,
                data as unknown as UserQuestionRequest,
              );
              break;
            }
            case "user_question_resolved": {
              upsertUserQuestionPart(
                parts,
                toolIndexes,
                data.request as UserQuestionRequest,
                data.response as UserQuestionResponse,
              );
              break;
            }
            case "supervisor_progress": {
              supervisorIndex = appendSupervisorProgress(parts, supervisorIndex, runId, data);
              break;
            }
            case "supervisor_report": {
              supervisorIndex = applySupervisorReport(parts, supervisorIndex, runId, data);
              break;
            }
            case "sub_session": {
              appendSubSessionEvent(parts, subSessionIndexes, runId, event.sequence, data);
              break;
            }
            case "context_compaction": {
              upsertContextCompactionPart(
                parts,
                toolIndexes,
                data as unknown as ContextCompactionEvent,
              );
              break;
            }
            case "steer_injected": {
              const count = Number(data.count ?? 1);
              const preview = String(data.preview ?? "");
              parts.push({
                type: "text",
                text: `\n\n[Steer injected: ${count}${preview ? ` - ${preview}` : ""}]\n\n`,
              });
              break;
            }
            case "stats": {
              onStatsChanged(data as DebugStats);
              break;
            }
            case "todo_state": {
              onTodosChanged((data.items ?? []) as TodoItem[]);
              break;
            }
            case "session_forked": {
              onSessionForked(data as Session);
              break;
            }
            case "error":
              throw new Error(String(data.message ?? "run failed"));
          }
          updateAssistant({ type: "running" });
        }
        updateAssistant({ type: "complete", reason: "stop" });
      } catch (error) {
        if (controller.signal.aborted) {
          updateAssistant({ type: "incomplete", reason: "cancelled" });
        } else {
          updateAssistant({
            type: "incomplete",
            reason: "error",
            error: String(error),
          });
          throw error;
        }
      } finally {
        runningToolIds.clear();
        stopTicker();
        if (activeRunRef.current?.id === runId) {
          failPendingSteers(
            activeRunRef.current,
            controller.signal.aborted
              ? "run cancelled before steer could be sent"
              : "run finished before steer could be sent",
          );
          activeRunRef.current = undefined;
        }
        setIsRunning(false);
        onFinished();
      }
    },
    [
      failPendingSteers,
      flushPendingSteers,
      onFinished,
      onSessionForked,
      onStatsChanged,
      onTodosChanged,
      sendSteer,
      sessionId,
    ],
  );

  const onCancel = useCallback(async () => {
    const run = activeRunRef.current;
    if (!run) return;
    run.controller.abort();
    await api.cancelRun(run.id).catch(() => undefined);
  }, []);

  const adapter = useMemo(
    () => ({
      messages,
      isRunning,
      onNew,
      onCancel,
      convertMessage: (message: ThreadMessageLike) => message,
    }),
    [isRunning, messages, onCancel, onNew],
  );
  const runtime = useExternalStoreRuntime(adapter);
  return <AssistantRuntimeProvider runtime={runtime}>{children}</AssistantRuntimeProvider>;
}
