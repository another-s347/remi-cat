export type Session = {
  id: string;
  title?: string | null;
  channel_binding: { platform: string; channel_id: string };
  created_at: string;
  updated_at: string;
  active_run?: ActiveRun | null;
  sort_at?: string;
};

export type HistoryMessage = {
  id: string;
  role: "user" | "assistant" | "tool";
  text: string;
  timestamp?: string | null;
  tool_call_id?: string | null;
  tool_calls?: HistoryToolCall[] | null;
  pretty?: PrettyToolCall | null;
};

export type HistoryToolCall = {
  id: string;
  type: string;
  function: {
    name: string;
    arguments: string;
  };
};

export type ChatEvent = {
  version: 1;
  event: string;
  run_id: string;
  session_id: string;
  sequence: number;
  timestamp: string;
  data?: Record<string, unknown>;
};

export type ActiveRun = {
  session_id: string;
  run_id: string;
  text: string;
  started_at: string;
};

export type TodoItem = {
  id: number;
  content: string;
  description?: string | null;
  done: boolean;
  batch_id?: number | null;
  batch_title?: string | null;
  batch_index?: number | null;
};

export type DebugStats = {
  ttft_ms?: number | null;
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;
  max_prompt_tokens?: number;
  context_tokens?: number;
  context_usage?: number;
  model_elapsed_ms?: number;
  elapsed_ms?: number;
};

export type ModelInputCategory =
  | "system_prompt"
  | "skill_injection"
  | "history"
  | "tool_input"
  | "tool_output"
  | "current_user"
  | "metadata"
  | "user_state";

export type ModelInputTotals = {
  estimated_tokens: number;
  prompt_tokens?: number | null;
  completion_tokens?: number | null;
  total_tokens?: number | null;
  max_prompt_tokens?: number | null;
  context_tokens?: number | null;
};

export type ModelInputSegment = {
  id: string;
  category: ModelInputCategory;
  role?: string | null;
  title: string;
  content: string;
  token_estimate: number;
};

export type ModelInputSnapshot = {
  run_id: string;
  thread_id: string;
  model_profile_id: string;
  model: string;
  created_at: string;
  segments: ModelInputSegment[];
  totals: ModelInputTotals;
};

export type ModelInputSummary = {
  run_id: string;
  created_at: string;
  model_profile_id: string;
  model: string;
  segment_count: number;
  estimated_tokens: number;
  prompt_tokens?: number | null;
  completion_tokens?: number | null;
  total_tokens?: number | null;
  max_prompt_tokens?: number | null;
  context_tokens?: number | null;
};

export type ContextCompactionEvent = {
  id: string;
  thread_id: string;
  status: "started" | "completed" | "failed";
  source: "auto" | "manual";
  compacted_messages: number;
  remaining_messages: number;
  error?: string | null;
};

export type PrettyToolCall = {
  id: string;
  tool_name: string;
  title: string;
  summary: string;
  status: "running" | "success" | "error";
  elapsed_ms?: number | null;
  started_at_ms?: number | null;
  execution_started_at_ms?: number | null;
  preparation_elapsed_ms?: number | null;
  request: unknown;
  response?: string | null;
};

export type ToolRiskLevel = "low" | "medium" | "high";

export type ToolApprovalDecision =
  | "deny"
  | "allow_once"
  | "allow_same_command_session"
  | "allow_risk_level_session";

export type ToolRiskReview = {
  risk: ToolRiskLevel;
  reason: string;
  concerns: string[];
};

export type ToolApprovalRequest = {
  id: string;
  session_id: string;
  run_id: string;
  tool_call_id: string;
  tool_name: string;
  risk: ToolRiskLevel;
  args_summary: string;
  command_key?: string | null;
  model_review_reason?: string | null;
  platform?: string | null;
  review?: ToolRiskReview | null;
};

export type UserQuestionStatus = "answered" | "cancelled";

export type UserQuestionOption = {
  id: string;
  label: string;
  description?: string | null;
};

export type UserQuestionRequest = {
  id: string;
  session_id: string;
  run_id: string;
  tool_call_id: string;
  question: string;
  reason?: string | null;
  options: UserQuestionOption[];
  allow_free_text: boolean;
  placeholder?: string | null;
  default_option_id?: string | null;
  created_at: string;
};

export type UserQuestionResponse = {
  question_id: string;
  status: UserQuestionStatus;
  selected_option_ids: string[];
  free_text?: string | null;
  answer_text?: string | null;
  answered_at?: string | null;
  source?: string | null;
};

export type InputHistory = {
  items: string[];
};

export type SecretEntry = {
  key: string;
};

export type SecretList = {
  backend: string;
  entries: SecretEntry[];
};

export type CommandCatalogEntry = {
  value: string;
  label: string;
  description: string;
  keywords?: string[];
  accepts_arguments?: boolean;
};

export type WorkspaceFileMatch = {
  relative_path: string;
  mention_path: string;
  display_path: string;
  kind: "file" | "directory";
  size?: number;
  modified?: string;
};

async function json<T>(input: RequestInfo, init?: RequestInit): Promise<T> {
  const response = await fetch(input, init);
  if (!response.ok) {
    const payload = await response.json().catch(() => ({}));
    throw new Error(payload.error ?? `${response.status} ${response.statusText}`);
  }
  return response.json() as Promise<T>;
}

export const api = {
  listSessions: () => json<Session[]>("/api/v1/chat/sessions"),
  createSession: () =>
    json<Session>("/api/v1/chat/sessions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    }),
  renameSession: (id: string, title: string) =>
    json<Session>(`/api/v1/chat/sessions/${id}`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ title }),
    }),
  forkSession: (id: string, title?: string | null) =>
    json<Session>(`/api/v1/chat/sessions/${id}/fork`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ title }),
    }),
  deleteSession: async (id: string) => {
    const response = await fetch(`/api/v1/chat/sessions/${id}`, { method: "DELETE" });
    if (!response.ok) throw new Error(`delete failed: ${response.status}`);
  },
  history: (id: string) =>
    json<HistoryMessage[]>(`/api/v1/chat/sessions/${id}/messages`),
  modelInputs: (id: string) =>
    json<ModelInputSummary[]>(`/api/v1/chat/sessions/${id}/model-inputs`),
  modelInput: (id: string, runId: string) =>
    json<ModelInputSnapshot>(
      `/api/v1/chat/sessions/${id}/model-inputs/${encodeURIComponent(runId)}`,
    ),
  activeRun: (id: string) =>
    json<ActiveRun | null>(`/api/v1/chat/sessions/${id}/runs/active`),
  todos: (id: string) =>
    json<TodoItem[]>(`/api/v1/chat/sessions/${id}/todos`),
  inputHistory: (id: string) =>
    json<InputHistory>(`/api/v1/chat/sessions/${id}/input-history`),
  appendInputHistory: (id: string, text: string) =>
    json<InputHistory>(`/api/v1/chat/sessions/${id}/input-history`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ text }),
    }),
  cancelSessionRun: (sessionId: string) =>
    fetch(`/api/v1/chat/sessions/${sessionId}/runs`, { method: "DELETE" }),
  steerRun: async (sessionId: string, runId: string, text: string) => {
    for (let attempt = 0; attempt < 8; attempt += 1) {
      const response = await fetch(`/api/v1/chat/sessions/${sessionId}/steers`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ run_id: runId, text }),
      });
      if (response.ok) return;
      if (response.status === 409 && attempt < 7) {
        await sleep(75);
        continue;
      }
      const payload = await response.json().catch(() => ({}));
      throw new Error(payload.error ?? `steer failed: ${response.status}`);
    }
  },
  cancelRun: (runId: string) =>
    fetch(`/api/v1/chat/runs/${runId}`, { method: "DELETE" }),
  decideApproval: (id: string, decision: ToolApprovalDecision) =>
    json<ToolApprovalRequest>(`/api/v1/chat/approvals/${id}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ decision }),
    }),
  answerUserQuestion: (
    id: string,
    body: {
      selected_option_ids?: string[];
      free_text?: string | null;
      source?: string;
      cancel?: boolean;
    },
  ) =>
    json<UserQuestionRequest>(`/api/v1/chat/user-questions/${id}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),
  listSecrets: () => json<SecretList>("/api/v1/secrets"),
  setSecret: (key: string, value: string) =>
    json<SecretList>("/api/v1/secrets", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ key, value }),
    }),
  deleteSecret: (key: string) =>
    json<SecretList>(`/api/v1/secrets/${encodeURIComponent(key)}`, {
      method: "DELETE",
    }),
  commands: () => json<CommandCatalogEntry[]>("/api/v1/chat/commands"),
  fileMatches: (query: string, limit = 8) =>
    json<{ items: WorkspaceFileMatch[] }>(
      `/api/v1/chat/files?q=${encodeURIComponent(query)}&limit=${limit}`,
    ),
};

export async function* streamRun(
  sessionId: string,
  runId: string,
  text: string,
  signal: AbortSignal,
  onOpen?: () => void,
): AsyncGenerator<ChatEvent> {
  const response = await postRun(sessionId, runId, text, signal);
  if (!response.ok || !response.body) {
    const payload = await response.json().catch(() => ({}));
    throw new Error(payload.error ?? `run failed: ${response.status}`);
  }
  onOpen?.();
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  while (true) {
    const { done, value } = await reader.read();
    buffer += decoder.decode(value, { stream: !done });
    const lines = buffer.split("\n");
    buffer = lines.pop() ?? "";
    for (const line of lines) {
      if (line.trim()) yield JSON.parse(line) as ChatEvent;
    }
    if (done) break;
  }
  if (buffer.trim()) yield JSON.parse(buffer) as ChatEvent;
}

function postRun(sessionId: string, runId: string, text: string, signal: AbortSignal) {
  return fetch(`/api/v1/chat/sessions/${sessionId}/runs`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ run_id: runId, text }),
    signal,
  });
}

function sleep(ms: number) {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}
