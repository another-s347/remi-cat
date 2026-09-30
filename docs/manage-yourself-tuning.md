# `manage_yourself` 场景调优

## 能力边界

`manage_yourself` 只接受 `{"command":"<remi-cat 参数>"}`。命令按 shell 引号规则拆词后直接调用当前宿主的 `remi-cat`，不经过 shell；单次执行超时为 90 秒。因此不要传管道、重定向或二进制名。当前会话的 `/model reasoning ...` 属于聊天 slash 命令。

| 用户目标 | 配置或状态层 | CLI 入口 | 验证方式 |
| --- | --- | --- | --- |
| 当前选中了哪个 Profile、为何选中 | 选择状态 | `profile current` | 输出中的选择来源 |
| 列出、搜索、检查 Profile | 注册表及 manifest | `profile list/find/show/check` | `--format json`、`--sources`、`--strict` |
| 新建、注册、修改 Profile | `profile.yaml` | `profile init/register/set/unset` | `profile show <ref> --sources --format yaml` |
| 修改运行时参数 | `runtime.yaml` | `--profile <ref> config set key=value`、`sandbox set`、`telemetry` | 检查目标 `config.runtime` 文件或诊断命令 |
| 选择长期默认模型 | `runtime.yaml` 的 `model_profile` | `--profile <ref> config set model_profile=<id>` | 检查保存结果及模型凭证校验 |
| 修改模型名、context 和输出预算 | `models/<id>.yaml` | `profile model show/set <ref> [id]`；省略 ID 指运行时所选模型 | CLI 校验并原子写入；重新加载运行时 |
| 修改自动压缩百分比 | `runtime.yaml` 的 `auto_compress_context_percent` | `--profile <ref> config set auto_compress_context_percent=70` | 检查文件及运行时环境 |
| 当前会话临时切换模型 | 会话状态 | `/model use <id>`，`/model reset` | `/model status` |
| 管理具体飞书连接器 | `channels.yaml` | `profile channel list/upsert-feishu/enable/disable/remove` | `profile channel list <ref> --format json` |
| 管理后台进程 | 实例记录 | `profile start/status/stop/restart` | `profile status <ref> --format json` |
| 管理 Agent 定义 | `agents/*.md` | `profile agent list/show/upsert/set-default` | `profile agent show <ref> <id>` |
| 管理 Supervisor Workflow | `workflows/*.json` | `workflow list/show/add/rm --profile <ref>` | `workflow show --profile <ref> <id>` |
| 查询工具及诊断 | 工具注册表 | `tools --json` | 检查 JSON 或错误；构造工具注册表可能需要模型凭证 |

`profile set capabilities.channels` 只写发现元数据，不会启用实际连接器。`profile find` 只搜索已注册或传统 Profile；直接给 `profile show` 一个未注册 manifest 路径仍可检查它。Agent 和 Workflow 命令现通过统一 Profile 注册表解析 `@alias`、`id:`、manifest 路径和传统名称。`profile agent` 现也直接出现在 `profile --help` 中。

`config set` 当前接受的主要字段包括 `root_agent_id`、`model_profile`、`auto_compress_context_percent`、`tool_output.overflow_bytes`、`tool_output.foreground_timeout_ms`、`tool_output.async_agent`、`sandbox.*`、`shell.mode`、`im.mode`、`acp.*` 和 `telemetry.*`。它只支持实现中列出的键，不能任意编辑 YAML。`profile set` 也只支持有类型的 manifest 字段。更复杂的模型、Agent、Workflow 文件需要按各自格式编辑并验证。

模型配置需要分清三层：

1. `runtime.yaml` 的 `model_profile` 指向一个模型 Profile ID。`config set model_profile=...` 会向该模型的 `/models` 接口检查凭证；失败时不能假定已保存。当前聊天的 `/model use <id>` 不修改这个进程默认值。
2. `models/<id>.yaml` 定义真正的 `model`、`provider`、`base_url`、`context_tokens`、`max_output_tokens`、`overflow_bytes`、`context_compaction`、可选的思考设置。已支持 `profile model show/set <ref> [id]` 对常见字段进行有类型的读取和修改；省略 ID 时解析 `runtime.yaml` 当前选择的模型。修改会先校验完整模型定义，再原子替换 YAML。`max_output_tokens` 必须小于 `context_tokens`；这只是本地配置校验，仍需与提供商实际窗口核对。其他尚无 CLI 选项的字段仍需文件工具。
3. 自动压缩阈值是 `auto_compress_context_percent`，范围 1–100，未配置时默认 80%，也可用进程环境变量 `REMI_AUTO_COMPRESS_CONTEXT_PERCENT`。它控制何时按模型的 `context_compaction` 策略处理上下文，不会扩大模型窗口。`tool_output.overflow_bytes` 是工具输出阈值，意义不同。修改正在运行的托管 Profile 后需重启相应实例。

## 数据与评测

[`scripts/manage_yourself.jsonl`](../scripts/manage_yourself.jsonl) 有 63 条手工场景：57 条可执行参考操作，6 条安全或能力边界用例。新增内容覆盖模型选择、模型名、64K context、最大输出、无效预算、压缩策略、自动压缩百分比、Agent 模型绑定、飞书 Event Hook，以及有类型的模型 CLI 和无效模型检查。每条记录包含用户表述、类别、参考命令以及可选的文件编辑、环境准备与验证。它是命令选择与行为回归数据集，不是模型微调语料，也不是所有提供商的行为证明。

隔离 CLI 回归：

```sh
python3 scripts/eval_manage_yourself.py --output /tmp/remi-manage-cli.jsonl
```

评测器为每条用例创建独立临时 HOME、数据目录和注册表；配置好虚拟 API key 后执行参考命令或修改临时模型文件，并检查退出码及关键输出。模型切换用例使用本地模拟 `/models` 接口，不会调用真实提供商。当前结果：57 条可执行用例全部通过，6 条边界用例标记为 `policy_only`。

真实模型工具选择烟测：

```sh
python3 scripts/eval_manage_yourself_live.py \
  --model-profile-file .remi-cat/models/kimi-k2.6.yaml \
  --model-profile-id kimi-k2.6 \
  --credential-env KIMI_API_KEY --dotenv .env \
  --output /tmp/remi-manage-live.jsonl
```

这个评测只允许挑选预设的只读场景，用 `--permissions low` 启动独立 Profile，并记录 `manage_yourself` 实际发给 CLI 的参数；只答对而没调用相应命令不会通过。默认运行 `discover_profiles` 和 `current_profile`，可重复传 `--case` 增加只读场景。认证变量只进入子进程环境，不写入结果文件。可换用其他模型 Profile 和对应凭证变量。运行真实模型会消耗相应提供商调用额度。

明确授权的临时修改烟测可加 `--allow-mutations` 并选取场景：

```sh
python3 scripts/eval_manage_yourself_live.py \
  --model-profile-file .remi-cat/models/kimi-k2.6.yaml \
  --model-profile-id kimi-k2.6 \
  --credential-env KIMI_API_KEY --dotenv .env \
  --allow-mutations \
  --case change_context_window \
  --case set_compaction_threshold \
  --case upsert_feishu_event_hook
```

评测器为每条请求新建临时 Profile，并将 Agent 工具限制在查询、`manage_yourself` 和文件编辑相关工具。它检查实际文件值；context 修改后还会重新加载工具注册表验证模型 YAML。三条场景已分别通过。第一次“64K”请求被模型写成 64000，而参考答案是 65536；把请求明确为“65536 token”后通过，因此这项用例现在考察明确数值的执行，不把单位歧义当作模型失败。

连续轮次可传 `--rounds 2` 或更高值。评测器在同一个持久化 CLI channel 中先执行请求，再让模型重新读取目标 Profile 并核实结果。每轮都要求进程成功退出、产生非空可见答复；部分场景还核对答复中的关键实际值。复核轮另要求至少一次成功的 `manage_yourself` 调用；首轮仍检查具体命令及落盘状态。CLI 的 `prompt` 现在会在流未正常结束或没有可见答复时返回错误，避免空轮次被计为成功。例如：

```sh
python3 scripts/eval_manage_yourself_live.py \
  --model-profile-file .remi-cat/models/kimi-k2.6.yaml \
  --model-profile-id kimi-k2.6 \
  --credential-env KIMI_API_KEY --dotenv .env \
  --allow-mutations --rounds 2 \
  --case current_profile --case inspect_sources --case list_channels \
  --case change_context_window --case set_compaction_threshold \
  --case upsert_feishu_event_hook \
  --output /tmp/remi-manage-live-rounds.jsonl
```

工具子进程现在会继承当前 manifest Profile；显式 `--profile` 参数仍优先。之前真实模型能正确调用 `profile current`，但子进程因使用默认 Profile 而返回错误 ID；修复后同一用例报告 `eval.profile`。连续轮次评测是模型行为的样本检查，成功工具调用和非空答复仍不能单独证明答复内容全部正确，因此结果应结合记录中的答复和落盘配置审阅。

后续可靠性改进：`profile check` 现在把 Agent/模型定义加载错误计入失败；`runtime.yaml` 使用同目录临时文件同步写入并原子替换，避免写入中断时截断旧文件。`profile model set` 对模型名、context、最大输出、模型输出溢出阈值和压缩策略提供有类型的批量修改，校验失败不会动原文件，支持 `--dry-run`。模型输出溢出参数名为 `--model-overflow-bytes`，与全局工具输出的 `--overflow-bytes` 区分。CLI `prompt` 也要求模型完成事件或明确的直接回复，而不只看外层流结束事件。

第一次 6 场景 × 2 轮真实 Kimi 测试有 4 场景通过。context 场景实际成功，但评测器误查了 `default.yaml`，现已改为检查当前模型文件。另一次 context 复测发现，模型只把 `context_tokens` 降至 65536，保留了 131072 的 `max_output_tokens`，使第二轮启动失败；在内置技能、工具参数说明及评测提示中强调联动校验后，复测两轮通过，实际保存了 65536 和 32768，且 `tools --json` 加载成功。阈值场景已在首轮通过 `config set` 写入 70 并加载成功，但第二轮出现过只有工具调用、没有可见答复的失败；CLI 现在对此返回非零退出码。这个失败尚不能宣称已解决，需继续重复实测。

后续六场景完整复跑为 5/6：当前 Profile、路径来源、具体 Channel、context 与输出预算、70% 阈值都连续两轮通过。Event Hook 的配置正确，但泛化复核只答了禁用状态和传输类型，漏报端口；要求第二轮读取 `channels.yaml` 并逐项报告 host、port、path 后，单项两轮复测通过。因此六类场景都有真实 Kimi 两轮通过样本，但不是同一次 6/6 批量通过，也不能由少量样本推断稳定成功率。尤其阈值的泛化复核曾多次空答，提示设计对完成率有明显影响。评测器保留空答、关键值缺失和工具调用失败为失败，不以配置写入成功掩盖轮次失败。

新增有类型的模型 CLI 后，`change_context_window` 的低权限两轮复测起初又出现空答。跟踪显示第二轮的只读 `profile model show` 被误分为中风险并拒绝；风险分类现将它标为低风险，而 `profile model set` 保持中风险。重建后单项两轮真实 Kimi 复测通过：第一轮写入当前选中模型的 `context_tokens: 65536` 与 `max_output_tokens: 32768`，第二轮成功读回两项且给出非空正确答复。首轮曾尝试不存在的 `@eval.profile` 别名，随后自行恢复并完成操作；这说明命令选择仍有减少试错的空间。此结果是一次单项通过样本，不代表跨模型稳定性。

首次真实烟测中，模型对 `discover_profiles` 调用了 `profile list`，对 `current_profile` 调用了 `profile show`；后者能给出 ID，但缺少 `profile current` 的选择来源信息。把两者区别写入内置 `remi` skill 和工具描述后，同样两条场景的实际调用为 `profile list`、`profile current`，均通过。扩展烟测又暴露了资源、Channel、实例、注册表以及来源解释的命令选择问题；增加针对性提示后，这五条在各自复测中均选择了预期的成功命令。评分器也已修正为仅认可退出码为 0 的调用，并检查 `--sources` 等关键选项。这些结果是单次 Kimi Profile 烟测；还需要更多样本和其他模型才能衡量稳定性。

以上代码和评测运行在仓库构建的 `target/debug/remi-cat` 上；没有修改现有用户 Profile，也没有替换已安装或正在运行的二进制。`cargo fmt --check` 仍会列出工作区中其他未提交文件的格式差异，因此没有对整个工作区执行自动格式化。
