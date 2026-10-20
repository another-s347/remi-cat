# ChatGPT subscription models

remi-cat can use an eligible ChatGPT subscription through OpenAI's public Responses API. This is OAuth authorization for ChatGPT plan usage; no API key is required. Availability and model access depend on the account, workspace and OpenAI rollout. This integration does not read ChatGPT conversations.

## Connect and select a model

### 一次完成登录和配置

```sh
remi-cat chatgpt setup
remi-cat tui
```

`setup` 会显示正在配置的实例和凭据目录，自动打开浏览器。用户在浏览器中登录并授权后，回到终端选择模型。remi-cat 会发起一次真实推理，收到非空回复并成功结束后才保存默认模型。无需 API Key、客户端密钥或手工编辑 YAML，也无需向别人发送授权链接、回调 URL 或授权码。已有授权会直接复用。已有其他用途的同名模型不会被覆盖，可用 `--id` 换一个 ID。

WSL 会自动调用系统自带的 Windows PowerShell，在 **Windows 的本机回环地址** 接收浏览器回调，再通过父子进程管道交回 WSL。默认选择空闲端口；无需修改浏览器地址、WSL IP、防火墙、Windows 端口转发或安装额外代理。成功、取消和超时会关闭临时监听。Windows 互操作不可用时会在打开授权页面前明确报错，并提供本机回调/SSH 转发方案。

指定模型并跳过选择菜单：

```sh
remi-cat chatgpt setup --model gpt-6-luna
```

setup 默认使用 32768 token 的保守本地上下文预算和 4096 token 的输出预留，不代表模型的最大容量或服务端输出上限。需要其他预算时可传 `--context-tokens` 和 `--max-output-tokens`；模型必须在当前账户的模型目录中可见。已有同 ID、同 slug 的订阅模型配置会保留。

Without `--profile`, ChatGPT commands use the same user data directory as the TUI (`~/.remi_cat` if present, otherwise the existing `~/.remi-cat`, or the normal TUI default). An explicit `REMI_DATA_DIR` still selects a different runtime for these CLI commands. For a named profile, add the same `--profile @work` selection to every command. Manifest paths remain explicit. Each resolved runtime data directory has one ChatGPT registration. Profiles deliberately configured to share that data directory also share the registration. The CLI prints the resolved locations during login/setup; authorization is never silently copied between profiles.

### Separate login and model management

```sh
remi-cat chatgpt login
remi-cat chatgpt status
remi-cat chatgpt models
```

Choose a model slug from that account's catalog. Supply its documented context capacity and your output reservation; remi-cat does not guess these from the model name.

```sh
remi-cat chatgpt model add <slug> --id chatgpt-main --context-tokens <context-capacity> --max-output-tokens <output-reservation>
remi-cat chatgpt check --model-profile chatgpt-main
```

The check consumes subscription usage and succeeds only after a non-empty reply and `response.completed`. Listing models alone does not establish inference access. Login and model creation leave the default unchanged. On an initialized profile, add `--set-default` when creating the model to select it, or use the existing config command:

```sh
remi-cat config set model_profile=chatgpt-main
```

An existing chat can use `/model chatgpt-main`. TUI, CLI, Web, Feishu, ACP, local sub-agents, approval review and memory compression share the selected model/provider path. Sub-agents with their own model bindings retain those bindings. Independent API-key-only features, including decision-model APIs and external ACP providers, retain their own authentication.

## Remote terminals and login lifecycle

On a remote machine, first forward a chosen loopback port **from the machine running the browser**:

```sh
ssh -L 1455:127.0.0.1:1455 user@server
```

Then, in that SSH session, run:

```sh
remi-cat chatgpt setup --callback-host local --no-browser --port 1455
```

Open the URL displayed in **your own terminal** in your browser. The callback is carried over SSH and consumed by the same pending login process. Keep that terminal open. The CLI waits up to thirty minutes; restarting login creates a fresh state/nonce/PKCE attempt, so old browser tabs cannot complete a new attempt. There is no device-code flow and no workflow involving sending a callback URL to another person.

`--callback-host auto` (default) selects Windows for a local WSL session, and local loopback elsewhere. An SSH session does not automatically open a remote browser or start the WSL Windows helper. Use `--callback-host local` to explicitly receive on the current machine, or `--callback-host windows` to explicitly select WSL Windows interoperability.

If a specified port is unavailable or Windows reserves it, omit `--port` for local login so a free port is selected. If WSL interoperability is disabled, enable it in the user's WSL configuration or use local loopback with a proper SSH tunnel. A Windows listener failure ends the attempt before browser authorization; there is no silent fallback to an unreachable WSL address. `--no-browser` prints the link instead of opening it and still waits for the standard loopback callback.

Tokens live under the resolved runtime's `auth/chatgpt/`, outside model YAML and dotenv. Unix directories/files use owner-only permissions; Windows uses a current-user directory ACL. Refreshes are serialized across processes. Applications reload credentials for each inference request; a restart is not needed after refresh or logout.

```sh
remi-cat chatgpt logout
```

Logout attempts remote revocation and clears local tokens even if revocation cannot be confirmed. Already-running HTTP requests may finish. New requests fail until login. Host ID and client registration are retained. While signed in, reauthorization must match the existing account. To replace the single saved account, sign out and then run `chatgpt login --new-account`; ordinary login reuses the existing registration.

## Preview constraints

- Model profiles use `provider: chatgpt`. Custom API destinations are rejected. All subscription inference uses `https://api.openai.com/v1/responses`.
- Requests use HTTP/SSE, `store: false`, and client-supplied history. Text and user images are supported; the model must accept images and the profile must enable `supports_images`. The CLI defaults that capability to false unless the catalog explicitly declares it; edit the model YAML if necessary.
- Local tools are sent as namespaced function calls and run under remi-cat's existing permissions. Hosted image generation, File Search, Code Interpreter, hosted MCP, audio/video input and Responses tool search are not enabled.
- `reasoning_effort` maps to Responses reasoning options. Other extra request options are rejected. `max_output_tokens` is a local context reservation, **not a server-enforced generation cap** on this route.
- Encrypted reasoning items needed for tool continuation are saved as opaque protocol state under the private auth directory, partitioned by registration and model. They survive restart and are never shown as reasoning text. Conversation history stays in remi-cat's existing memory/session storage.
- A failed, incomplete, cancelled or disconnected stream does not count as successful completion. Usage-limit and account-policy errors do not switch to paid API billing. Usage is managed at [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage); remi-cat does not estimate remaining subscription credits.

Official contracts: [registration and sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in), [refresh and logout](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions), [inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference), [preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

## 工具风险审核模型

未指定审核模型时，ChatGPT 主模型和切换到的 ChatGPT 模型自动使用 `gpt-6-luna` 审核工具风险。审核通过 Responses API 完成，共用当前实例的订阅授权，无需创建另一个模型 YAML、登录另一个账户或配置 API Key，也不依赖 Decisions API。主模型的推理强度和其他额外参数不会复制给 Luna。

已有的显式审核绑定仍优先，例如 `REMI_APPROVAL_MODEL_PROFILE=<已有模型配置ID>` 或嵌入式运行时的 `approval_model_profile(...)`。显式启用的实验性 JEV 审核继续使用其专用配置；其他 Provider 的既有默认行为保持不变。

这只选择审核模型，不修改工具风险规则或会话审批策略。Luna 不可访问、调用失败、超时（30 秒）或返回无法解析的 JSON 时要求人工确认，不改用其他账户或付费 API。审核结果的 `model` 字段记录实际请求的模型；启动诊断日志也记录审核模型。

2026-10-20 验证：自动选择、会话模型切换、显式覆盖和 JEV 优先级均通过离线测试；现有订阅的真实 Luna 审核分别把 `pwd` 判为低风险、把合成的目录删除请求判为高风险。测试未执行这些命令。

## Live validation (2026-10-20)

Using one explicitly authorized ChatGPT subscription, without an API-key fallback:

- OAuth sign-in and account model catalog succeeded. Seven models were listed.
- `chatgpt check` using `gpt-6-luna` returned nonempty text and successful termination.
- Responses namespace function calls read a real temporary file containing an unpredictable identifier. Tool-result readback and a subsequent conversation turn both reproduced it correctly.
- The Rust provider factory and AgentLoop independently executed a real file tool and returned its contents. `LlmCompressor` used the same subscription provider and preserved the identifier in a nonempty summary.
- The live stream exposed two integration issues, now fixed: terminal events may have an empty `output` array despite completed tool-item events, and compression must explicitly select its configured model in AgentConfig.
- A valid `POST /v1/decisions` request using `gpt-6-luna` and the same OAuth token returned HTTP **401**, error type `rejected_by_access_enforcement`, code `no_matching_rule` (request ID `req_9564ac8d865946e78503bb9b6000e049`). This account's subscription authorization could access Responses but could not access Decisions at test time. This does not establish availability for other accounts or future rollouts. No paid API key was tried.

Refresh rotation, concurrent refresh, logout invalidation, encrypted-state restart recovery, images and failure paths have offline coverage. Real token refresh, full persisted-session restart, child agents and individual chat-channel acceptance remain unverified with this account.

Code validation: debug build and changed-file formatting/diff checks passed. Core regression: 446 passed, 7 ignored, 2 filtered. The two filtered `rg_tool_accepts_json_style_escaped_path_quotes` and `rg_tool_accepts_structured_complex_args` tests also failed on the unmodified baseline in this environment. The explicitly enabled live AgentLoop/compression test passed separately.

To repeat the Rust live tool/compression test explicitly (consumes subscription usage):

```sh
REMI_CHATGPT_LIVE_DATA_DIR=/absolute/path/to/runtime-data cargo test -p bot-core live_subscription_agent_tool_and_compression --lib -- --ignored --nocapture
```

The live test uses an existing authorization, temporary fixture files and no API key. Ordinary test runs skip it. The user's existing default model is unchanged.

### Login UX validation (2026-10-20)

The setup/login follow-up passed 383 application tests (6 opt-in tests skipped), changed-file format checks and a debug build. A separately enabled WSL integration test used a real Windows loopback TCP listener and real Windows HTTP requests: an incorrect state returned 400, a valid synthetic callback arrived in Rust over the subprocess pipe, and the released port could be rebound. Complete CLI smoke tests independently verified declined authorization and Ctrl+C cancellation with temporary empty instances; both left no tokens and no Windows listener. They did not require browser authentication or use a real authorization code.

`chatgpt setup --model gpt-6-luna` was also run from outside the repository with the existing real authorization: it resolved the user's TUI data directory, fetched the model catalog, obtained a nonempty successful response and persisted the selected default. A fresh human browser consent through the new Windows helper has not been repeated; the earlier real consent and the new helper/CLI tests are separate evidence.

### v0.2.48 release checks

The complete workspace passed with `SHELL=/bin/bash cargo test --workspace --locked -- --test-threads=1`: 908 passed, 0 failed, 14 opt-in tests ignored, no filtered tests. This includes the two ripgrep cases previously failing under the default shell. A background Bash test that timed out under the initial parallel load also passed both independently and in this serial run. The no-default-features build check and changed-file formatting checks passed.
