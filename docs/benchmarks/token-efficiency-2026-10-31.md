# remi-cat / OpenCode / DeepSeek Harness token 效率实测

日期：2026-10-31。结论限定于下述单模型、小任务、独立会话实验。

**默认 remi-cat 没有比另外两套 harness 更费累计输入，但存在明确的优化空间：控制操作的续轮、闲置工具 schema、读文件预算不匹配，以及工具历史的重复发送。** 精简 profile 的累计输入下降 62.6%，未缓存输入下降 47.9%；关闭 Todo 的单独消融也有收益。两者都尚不足以证明应全局更换默认配置。

## 实验与证据

- 拉取源码：[OpenCode](https://github.com/anomalyco/opencode)，HEAD `8b4d98c33a75bc2404154486eadaffa8891493aa`；[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)，HEAD `ce7073d51254c26cd63b22a60835b8f948deb447`。checkout 在 `/tmp/remi-token-bench/upstreams/*-http1`。
- 实际执行发布包：OpenCode `2.2.5`，DeepSeek harness `0.2.1-rc.5`，Node `24.22.2`。DeepSeek 最新源码与发布包 CLI 不同，测试遵循发布包实际支持的参数；源码审查与发布包运行证据分开。
- remi-cat：本地 `0.2.50`，HEAD `9541d0018846f5b820f8023448e4743ede323e02`，重新 `cargo build -p remi-cat` 后运行 debug 二进制。包含工作区原有的 4 个修改文件；它们没有被本次工作修改。每组实验保存二进制 SHA-256 和当时 Git 状态。构建后 `agent.rs` 另出现并行修改，未触碰；四组实验的二进制 hash 全部一致，因此结果对应固定二进制，而非最后的工作区源码。
- 同一真实模型 `kimi-k2.6`，temperature `0.6`，thinking disabled，每次最多输出 `8192` tokens；配置上下文 `256000`。计量代理保存原始请求与实际发送请求，统一模型参数，主调用和辅助标题调用都计入。
- 三类任务：CSV 边界修复；跨模块实现/导出 retry 工具；从 3600 行日志和错误码表定位根因。每种默认 harness 各跑两次，18 次；精简 remi 跑 6 次，只关闭 Todo 跑 3 次，共 **27 次正式运行、342 次模型请求**。
- 每次使用相同初始文件、全新 HOME/profile/session 和独立工作目录。禁止委派。使用外部独立验收，CSV 5 项、retry 6 项、日志 1 项；原始实现均被拒绝，人工参考实现均通过。

完整证据：[summary.json](/home/skye/remi-cat/target/token-efficiency-2026-10-31/summary.json)，目录 `/home/skye/remi-cat/target/token-efficiency-2026-10-31/`，约 56 MiB。包含逐轮请求/响应/usage、命令、stdout/stderr、最终文件、自测及独立验收结果、profile、npm lockfile、版本记录。真实认证头不保存，已扫描实验文本，未发现 dotenv 中的真实凭据。

脚本：[README](/home/skye/remi-cat/scripts/token_efficiency/README.md)、[运行器](/home/skye/remi-cat/scripts/token_efficiency/run.py)、[任务与验收](/home/skye/remi-cat/scripts/token_efficiency/tasks.py)、[分析器](/home/skye/remi-cat/scripts/token_efficiency/analyze.py)。可提交的精简数据见同目录 `token-efficiency-2026-10-31.json`。

## 默认 harness 横向结果

以下全部是 **API usage 的实际值**，输入为每轮累计，含缓存；未缓存输入 = 输入 − 缓存读取。缓存输入是输入的子集，不可再次相加。

| Harness | 独立验收 | 累计输入 | 其中缓存 | 未缓存输入 | 输出 | 请求数 | 单任务耗时中位数 |
|---|---:|---:|---:|---:|---:|---:|---:|
| remi-cat 默认 | 5/6 | 512,773 | 401,408 | 111,365 | 8,814 | 81 | 78.6 s |
| OpenCode | 5/6 | 543,635 | 431,872 | 111,763 | 7,616 | 62 | 57.1 s |
| DeepSeek Harness | 5/6 | 789,705 | 638,976 | 150,729 | 13,615 | 98 | 96.3 s |

remi 默认累计输入比 OpenCode 少 **5.7%**，但未缓存输入只少 **0.4%**，输出多 **15.7%**，请求多 **30.6%**。因此不能把这 5.7% 宣称为费用节省。相对 DeepSeek harness，remi 的累计输入少 **35.1%**，未缓存输入少 **26.1%**。

各任务两次合计：

| 任务 | Harness | 验收 | 输入 | 输出 | 请求数 |
|---|---|---:|---:|---:|---:|
| CSV | remi | 1/2 | 180,979 | 3,460 | 30 |
| CSV | OpenCode | 1/2 | 212,726 | 3,633 | 29 |
| CSV | DeepSeek | 1/2 | 421,679 | 8,168 | 52 |
| retry | remi | 2/2 | 220,690 | 4,474 | 37 |
| retry | OpenCode | 2/2 | 130,027 | 3,084 | 21 |
| retry | DeepSeek | 2/2 | 178,410 | 4,436 | 33 |
| 日志定位 | remi | 2/2 | 111,104 | 880 | 14 |
| 日志定位 | OpenCode | 2/2 | 200,882 | 899 | 12 |
| 日志定位 | DeepSeek | 2/2 | 189,616 | 1,011 | 13 |

默认 remi 在 retry 任务明显多轮，日志任务则比两者少输入；不能用单个总排名解释所有任务。

三套默认配置的失败都是 CSV：remi 第二次和 OpenCode 第二次在末尾换行后多返回一个空行；DeepSeek 第一次把空输入解析为 `[['']]`。对应进程均退出 0，但独立验收失败。DeepSeek 第二次最终通过，花了 31 次请求、185.1 秒。**自测通过、进程正常结束和满足任务契约是三件事。**

## 输入、输出各部分花在哪里

为保留可比口径，各部分用同一个 `cl100k_base` 对实际序列化请求估算。它不是 Kimi tokenizer；下表是**估算构成比例**，不能解释成精确 API 分项账单，也不能乘上 usage 后假装精确分摊。

| Harness | 工具 schema | 系统提示 | 用户消息 | 助手历史（含工具参数） | 工具返回历史 |
|---|---:|---:|---:|---:|---:|
| remi 默认 | 53.1% | 16.5% | 1.6% | 13.3% | 15.5% |
| OpenCode | 26.0% | 35.2% | 1.2% | 8.2% | 29.4% |
| DeepSeek | 42.6% | 5.7% | 2.6% | 16.6% | 32.5% |
| 精简 remi | 36.8% | 4.7% | 3.6% | 29.0% | 25.8% |

首次主请求的实际输入：remi 4,551，OpenCode 6,035，DeepSeek 4,372，精简 remi 1,298 tokens，取各自 CSV 第一次。默认 remi 的固定起步成本并不比对手异常高，**但每增加一轮，固定提示、schema 和历史都会再发送一遍**。

默认 remi 首次主请求有 26 个工具。较大 schema 的 cl100k 估算：`manage_yourself` 515、`fs_read` 264、`fetch` 256、`agent__explorer` 244、`skill__get` 242、`ask_user_question` 227。本次主测没有调用 `manage_yourself`。精简 allowlist 只有 6 个指定工具，实际仍注入了 `external_agent_discover`、`external_agent_ask`，共 8 个；这也是后续核查工具注入边界的具体入口。

输出也主要不在最后的文字回答：

| Harness | 可见文本估算 | 工具参数估算 | 工具名估算 | API 实际输出 |
|---|---:|---:|---:|---:|
| remi | 832 | 6,894 | 181 | 8,814 |
| OpenCode | 857 | 6,125 | 59 | 7,616 |
| DeepSeek | 1,337 | 11,239 | 106 | 13,615 |
| 精简 remi | 1,413 | 7,841 | 122 | 10,125 |

构成估算不含完整协议开销，与 API tokenizer 不同。thinking 已关闭；API 仍为每次请求报告 1 个 reasoning token，实际 reasoning 合计分别 81、62、98、69，属于输出子集。没有可见 reasoning 文本。因此本轮无法比较启用深度思考后的推理效率。

### Todo 和控制操作

默认 remi 有 **20/81 次请求**紧跟在仅有 Todo 操作的结果后面，这些续轮累计携带输入 **121,726**，其中缓存 98,304，输出 3,394。这个数描述“Todo-only 结果之后发生了哪些请求”，**不是删除 Todo 后一定能节省的量**，因为模型可能重组后续步骤。

CSV 第一次的轨迹有 `todo__add`、4 次 `todo__complete`、1 次 `todo__update`；retry 第一次有新增和 4 次完成。DeepSeek 也有 Todo，12 次对应续轮累计输入 81,970；问题不是某个工具名，而是为少量状态变化频繁续轮。

### 大文件读取和历史重放

remi 日志第一次：默认 `fs_read` → 指定 `length=32768` 重读 → `length=16384` 重读 → `rg` 定位。前两次被输出上限拦住；随后 16 KiB 的日志内容约贡献 5,300 个 cl100k 估算 tokens，继续跟随后续请求重放。

源码可以对应这个行为：

- [fs_read 默认 32 KiB](/home/skye/remi-cat/crates/bot-core/src/tools/mod.rs:59)，工具定义也向模型描述这个默认值。
- 本次 remi profile `overflow_bytes: 24000`，与仓库 DeepSeek Flash profile 相同；[溢出处理](/home/skye/remi-cat/crates/bot-core/src/agent.rs:2704)超过上限后丢弃 `fs_read` 内容并返回缩小读取范围的提示。

`24000` bytes 实际约 23.4 KiB，比 32 KiB 默认读取小。由此会产生可避免的失败/重试。首先应把读取预算与有效输出预算对齐，而非提高所有工具的总上限。对于明确的 ID/错误码定位，应优先搜索，随后读命中附近。

OpenCode、DeepSeek 此题也先读大文件，所以它们的日志任务工具历史更大；本轮不支持“换成另一个 harness 就解决输出污染”的结论。

### 缓存、辅助请求、压缩

默认 remi / OpenCode / DeepSeek 的输入缓存比例约为 78.3% / 79.4% / 80.9%。精简 remi 是 69.7%，缓存比例下降但未缓存输入仍大幅减少。**缓存命中率、总 token、未缓存 token 和实际费用要分别看。** 本次没有刷新提供商缓存，没有建立美元/人民币成本模型。

OpenCode 标题辅助请求 6 次，输入 3,284、输出 73；DeepSeek 标题辅助请求 6 次，输入 1,484、输出 58，已包含在 totals 中。`auxiliary_candidates` 按无工具标题提示识别，原始请求可复核。正式运行的 342 次请求都有 HTTP 200 与 usage，未发现缺失 usage 或未结算请求；有工具失败、重读和自测修复，它们的成本都已计入。

任务没有触发长会话压缩，所以压缩支出在本次轨迹中未观察到，**不是零成本保证**。DeepSeek 源码有 model-free tool-result pruner；OpenCode 有预计算摘要和缓存余量策略，值得另做长会话、输出压力回归，不能用这批短任务证明其净收益。

## remi 消融：优化能获得多少

| 配置 | 运行数/验收 | 输入 | 未缓存输入 | 输出 | 请求数 | 耗时中位数 |
|---|---:|---:|---:|---:|---:|---:|
| 默认 remi，两轮 | 6 / 5 | 512,773 | 111,365 | 8,814 | 81 | 78.6 s |
| 精简 remi，两轮 | 6 / 6 | 191,680 | 58,048 | 10,125 | 69 | 76.5 s |
| 默认 remi，仅第一轮 | 3 / 3 | 303,172 | 67,652 | 4,943 | 47 | 88.0 s |
| 仅关闭 Todo，一轮 | 3 / 3 | 193,967 | 42,415 | 4,911 | 32 | 66.1 s |

精简 profile：缩短系统提示，指定 `bash/fs_read/fs_write/apply_patch/fs_ls/rg`，不提供 Todo。这是多个因素一起改变的实验。输入少 **62.6%**，未缓存输入少 **47.9%**，请求少 **14.8%**，但输出多 **14.9%**，耗时中位数只少约 **2.7%**。不能把 63% 输入降幅说成 63% 费用或时间降幅。

仅关闭 Todo：保留其他默认指令和工具，只移除 Todo 那一段指令及 Todo 工具。与默认第一轮同一组三题比较，输入少 **36.0%**、未缓存输入少 **37.3%**，请求从 47 降到 32；两组均 3/3。但只跑一次，是诊断性证据，不是稳定收益承诺。

以上改动只存在于临时 benchmark profile。**没有修改产品默认 agent、工具执行器或压缩器。**

## 建议实施顺序

1. **先补逐请求用量账本。** 当前 [TokenUsage](/home/skye/remi-cat/crates/bot-core/src/token_usage.rs:4)只有累计 prompt、completion、max prompt；应保留缓存读写、reasoning、请求类型、模型、父运行、失败/重试以及原始 usage。当前 [输入快照](/home/skye/remi-cat/crates/bot-core/src/runtime/mod.rs:3715)位于 inner agent 驱动之前，不能直接拿最初的分段估算解释整个多工具轮次的总账单。让真实请求边界的 snapshot 与 usage 一一对应。
2. **修正 fs_read 的预算冲突。** 默认读取长度、显式长度的提示和行范围读取都应考虑有效工具输出预算及封装余量；提供可继续读取的确定性范围。验证不丢内容、不循环重试，以及不同 model profile 的上限。
3. **让简单任务的计划状态更便宜。** 简单任务不强制 Todo；复杂任务批量提交状态变更，支持在同一轮并行组合无依赖的控制操作和业务工具。保留用户需要的进度信息，而不是统一移除计划能力。
4. **引入稳定的编码工具集合。** 自管理、外部 agent、记忆写入等按任务模式/明确发现结果启用。尽量在阶段边界选择工具，保持前缀和工具顺序稳定，防止动态开关频繁打破缓存。缩短 schema 说明需同时检查误调用率。
5. **再做工具历史保留策略。** 对搜索命中、错误、测试结论保留证据，旧的低价值大输出可使用可追溯的截断/外存引用。要先做模型无关的确定性处理，再评估额外 LLM 摘要是否值得。避免单纯降低上下文压缩阈值，以摘要调用替代原来的 token 开销。

先做 1、2 可以改善可观察性并消除确定的重复读取；3、4 已有消融证据支撑；5 需要增加长会话任务后再决定。

## 实验边界与准备阶段记录

- 单个模型、关闭 thinking、小型 Python/JSON 任务、每题仅两次，不能外推到复杂 Rust 改造、真实 remi 用户记忆、长会话或所有模型。精简 profile 的 6/6 不构成质量提升的统计证明。
- 并发执行以缩短测试时间；三套 harness 没有同时做严格冷缓存/同负载延迟实验。耗时只作描述，缓存与费用结论保留上述限制。
- retry 验收 v1 对 `delay=0` 强制要求调用注入的 sleep；任务文字没有明确要求这一点，过于严格。v2 改为正 delay 验证“仅在重试之间调用”。统一重验了所有最终文件，保留原结果和两版验收输出，没有向 agent 提供隐藏测试或重跑修改后的答案。
- npm 准备阶段补齐了 DeepSeek 包的 peer 依赖，并直接使用 OpenCode Linux 平台包，lockfile 已保留。源码 checkout 和发布包版本单独记录。
- 一次无效冒烟中，OpenCode 后台服务错误使用原仓库路径，在其中新建 `project/`；已停止该轮，将仅由该轮新建的目录移出原仓库，正式实验使用 standalone 和显式目录。该轮完全排除在正式结果外。remi 的默认 A2A 端口冲突通过临时进程使用端口 0 隔离。
- 正式 27 次实验合计输入 **2,231,760**、输出 **45,081** tokens。冒烟阶段另有 47 次返回 usage 的请求，输入 **390,195**、输出 **4,518**；两个 400 响应没有 usage。取消过程可能缺少提供商最终 usage，所以这些是已记录用量，不是完整账户账单。
- 验证：remi 当前源码完整构建成功；脚本编译检查、baseline/oracle 验收检查、27 份独立验收、342 次 usage 完整性核对、真实凭据扫描、`git diff --check` 完成。未提交或发布代码，原有 4 个修改文件保留。
