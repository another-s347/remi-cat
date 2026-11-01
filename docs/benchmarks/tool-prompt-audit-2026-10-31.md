# remi / OpenCode / dsh：工具定义与 prompt 横向审查

本次仅审查，未修改产品实现。依据已保存真实请求及源码：当前 remi（第二轮读取优化后）、OpenCode 2.2.5、DeepSeek Harness 0.2.1-rc.5。数字代表本次默认测试配置，不是软件支持工具的总数，也不是启用所有插件后的上限。

## 数量与体积

| 指标 | remi | OpenCode | dsh |
|---|---:|---:|---:|
| 顶层 function tools | 26 | 12 | 25 |
| 标准工具定义估算 tokens | 3,951 | 2,797 | 4,206 |
| system 文本估算 tokens | 1,250 | 3,422 | 519 |
| 首次 CSV 主请求 API 实际输入 | 4,752 | 6,035 | 4,372 |

估算统一使用 cl100k_base，对标准序列化工具定义和 system 文本计数；它不是 Kimi tokenizer，不含精确协议分摊，不能相加冒充实际账单。三个首次请求任务相同，但环境目录等动态内容有差异。

**remi 的数量与 dsh 相近，工具定义体积还略小；system 比 dsh 长，但远短于 OpenCode。本次起步输入没有异常膨胀。**

OpenCode 的 12 个顶层工具还包括 `execute`。system 中有一个约 **1,185 tokens** 的 Code Mode 区段，列出 **8 个**可在 execute 内调用的管理方法，包括模型发现、会话和 worktree 操作、MCP 资源。因此“12 对 26”不能直接解释为能力复杂度或总提示成本减半：一部分目录从 function schema 移进了 system。

dsh 的 system 很短，但工具描述承担了较多指导；单个 `workflow` 定义约 742 tokens、`bash` 约 478。remi 最大的 `manage_yourself` 约 515，也不是三者中异常大的单个定义。

## 能力覆盖与重复设计

| 能力 | remi | OpenCode | dsh | 判断 |
|---|---|---|---|---|
| 文件读写/修改 | fs_read、fs_write、apply_patch | read、write、edit | read、read_image、write、edit | 新建/覆盖与局部修改互补；不能仅凭覆盖就删工具 |
| 路径/内容发现 | fs_ls、rg；fs_read 也支持目录 | glob、grep；read 也支持目录 | glob、grep；目录查询也可经 bash | 文件阅读与目录浏览有重叠，但不是 remi 独有 |
| 通用执行 | bash | shell、execute | bash、workflow | 都有能覆盖专用工具的通用执行器；仍保留专用接口以明确参数和结果 |
| 技能 | search(scope=skills)、skill__search、skill__get | skill 加 system 技能目录 | skill 加会话技能目录 | remi 有两个搜索入口，是最明确的表面重复 |
| Todo | add/list/complete/update/remove，5 个 | 此次配置未提供顶层 Todo 工具 | todo_write，1 个 | remi 粒度细；dsh 一次提交完整列表。工具数量与总更新成本不同 |
| 子代理 | agent__explorer；external_agent_discover/ask | subagent | subagent/fork、list、send、interrupt、workflow | remi 的配置代理与外部 Profile/A2A 边界不同，但对模型的选择规则不够集中 |
| 后台任务 | tool_tasks，1 个多操作入口 | 部分能力由 shell 等工具承担 | job_list/output/kill，3 个 | remi 在这里反而更集中，不能只按工具个数判断设计质量 |
| 自管理 | manage_yourself | execute 后的 Code Mode 目录 | 多个 goal、plan、working_directory 等专用接口 | 三者都在用不同方式暴露管理能力 |

### 明确重复：技能搜索入口

`search(scope=skills)` 与 `skill__search` 都调用 `SkillStore.search(query)`。它们在结果格式、数量限制、诊断和空查询支持上有差异，因此不是完全相同的 API，但对模型的主要意图重叠。

默认 system 要求使用统一 `search`，pinned-skill 提示又建议 `skill__search → skill__get`，工具描述也分别引导搜索。建议对模型确定一个主要发现入口，旧接口可保留兼容，且需要合并保留目录列举与诊断功能。

`skill__search` 本身只有约 84 tokens，所以这个整理主要改善决策一致性，不是几千 token 的直接节省。

### Todo：可收敛，但不能只追求五变一

remi 五个定义合计约 422 tokens，dsh 的 `todo_write` 约 189。dsh 每次重传整份任务列表，remi 现已能增量批量提交 `ids`，并保留持久批次清理语义。直接替换为整表覆盖可能增加输出、改变批次行为。

可以考虑集中模型入口或按模式暴露操作，但应先看控制轮次、更新参数量和生命周期正确性。上一轮批量完成已经解决了部分行为成本，无需为减少工具名字而立即重构底层存储。

### 代理：有不同执行边界，问题在选择指导

`agent__explorer` 是预配置的命名子代理会话；`external_agent_discover/ask` 针对本地其他 Profile 或 Hub 的 A2A 代理。功能并不相同。

不过默认提示把大量普通仓库探索都导向 explorer。OpenCode 的提示是“可以委派 focused subtask”；dsh 的主提示强调已经决定委派时并行启动，workflow 仅限明确需要的场景。remi 的触发条件更积极，可能令本可在主会话完成的小探索产生额外模型会话。

这是源码规则显示的潜在流程成本；原 benchmark 明确禁止委派，不能声称该成本已经在此前 token 总数中得到测量。

## prompt 冗余在哪里

### remi

- 简洁、直接、不要铺垫、不要复述、不要过度解释、不要模板开场分成多条，可适度合并；绝对体积不大。
- 个人记忆必须搜索，在主 prompt 和 search 描述里重复。技能搜索在主 prompt、pinned 提示和多个工具描述中分散，且入口不一致。
- 文件编辑、Todo 完成确认、先搜索再读文件同时出现在主规则和工具说明。少量关键契约重复有价值；应明确规则由哪一层维护，避免多处修改后不一致。
- `manage_yourself` 的 description 和 command 参数里有大量 CLI 示例与模型配置说明，并夹带技能 pin、用户 slash command 等旁支信息。约 515 tokens，适合拆清“调用契约”和“按需手册”。

**system 只有约 1,250 估算 tokens，不支持大幅裁剪默认 profile 的结论。** 优先统一策略和入口，避免通过删掉关键条件诱发更多重试。

### OpenCode

system 更长，除了约 1,185 tokens 的 Code Mode 区段，还在 Prompt and Tool Use、General Guidelines for Coding 和 Ultimate Reminders 多次强调实际使用工具完成修改、不要只输出代码，以及修改后验证。

它把部分功能文档集中到 system，对齐了工具入口，但同样存在文字重复。不能把它当成“所有说明都更精简”的范例。

### dsh

主 system 约 519 tokens，较集中地说明工具选择、退出码、委派和 workflow 条件；较长的操作说明放在各工具。整体更接近“主规则短、工具契约具体”的分工。

代价是 workflow、后台任务、目标及多代理管理表面仍较大。短 system 不代表总输入很低，任务是否实际需要这些能力仍需考虑。

## 新发现：extra_prompt 不能直接算成模型 prompt

remi 的 `skill__get` 请求对象有一个非标准 `function.extra_prompt`，其中携带 local skill catalog。此前对完整 JSON 的估算把它包含在工具体积内。

本次对完全相同的 Kimi 请求只移除该字段，做了两个成功的真实 API 探针：

| 请求 | prompt_tokens |
|---|---:|
| 移除 extra_prompt | 4,752 |
| 保留 extra_prompt | 4,752 |

另一次保留字段的请求遇到 429，没有 usage，不用于判断。探针每次最多输出 32 tokens，未执行任何模型生成的工具调用。

因此在**这条 Kimi 路径**上，该字段没有增加 provider 报告的 prompt_tokens。原始工具 JSON 估算 4,127，移除私有字段后标准定义约 3,951；`skill__get` 本身约 66，而非包含目录字段时的 242。

这应当作为适配器/信息投递问题审查：目录是否真的进入模型上下文要有证据。不能说“删掉 extra_prompt 就节省 176 个真实输入 tokens”，也不能为了修复目录投递而未经评估把所有技能目录塞进 system。其他 provider 路径未在此次探针中验证。

## 建议顺序

1. 统一技能发现入口及 pinned 提示，并明确技能目录的真实投递位置。
2. 将自管理定义中的旁支说明归回相应技能/帮助，保留简洁但完整的调用契约。按需加载会增加一次读取和后续历史，需测全任务成本，不能预先承诺省 token。
3. 合并主 prompt 中的重复表达，集中策略维护；保留验证、权限和完成条件。
4. 根据实际任务评估代理触发条件及默认管理能力，保持阶段内工具集合稳定以利于缓存。
5. Todo 入口收敛列为后续评估；先衡量批量更新效果和真实使用频率，保留现有批次语义。

不建议把所有文件操作塞进 bash，也不建议只为把 26 降到 12 而做一个巨大 action 工具。当前优先问题是入口边界、契约归属和行为诱导，而非单纯数量过多。

证据：[definitions.json](/home/skye/remi-cat/target/tool-prompt-audit-2026-10-31/definitions.json)、[extra_prompt 探针](/home/skye/remi-cat/target/tool-prompt-audit-2026-10-31/extra-prompt-probe.json)。定义与 system 来自此前保存的完整实际请求，探针没有记录认证头。此次没有改动产品代码。
