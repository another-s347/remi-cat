# 工具入口与提示精简（2026-10-31）

已实现用户指定的三项调整。

- 默认 Todo 五个 CRUD 定义收敛成 `todo(action=add/list/complete/update/remove)`；仍支持增量更新、批量完成、批次清理和跨重启持久化。旧处理器保留给历史调用；完整旧白名单自动映射，部分权限不扩大。同步了审批分类、Todo 事件、展示文案和 doctor 检测。
- 在白名单过滤后，如果模型实际能使用 `search`，不展示 `skill__search`；没有统一搜索时保留专用入口。pinned-skill 提示使用对应路线，两个分支均验证实际 HTTP 请求。
- `manage_yourself` 定义只保留调用契约与技能入口，CLI 示例和配置说明归入内置 `remi` 技能。默认与回退 system prompt 均引导先读取技能。

## 定义体积

从相同编码任务的第一份真实请求提取；采用 `cl100k_base`，移除非标准 `function.extra_prompt` 后估算标准工具 JSON。不是 Kimi 账单 tokenizer。

| 部分 | 修改前 | 修改后 |
|---|---:|---:|
| 模型展示工具数 | 26 | 21 |
| Todo 定义合计 | 422 | 186 |
| 自管理定义 | 515 | 101 |
| 技能专用搜索定义 | 84 | 0（统一 search 已存在） |
| 全部标准工具定义 | 3,951 | 3,218 |
| 首请求 system 文本 | 1,250 | 1,246 |

工具定义估算减少 733 tokens，约 18.6%。旧定义不会在同一模型请求里与统一 Todo 重复发送。诊断注册表仍保留旧接口，便于识别旧配置。

## 真实模型验证

模型与参数保持此前 Kimi K2.6 基准设置：temperature=0.6、thinking disabled、max output=8192。CSV、retry、logs 三项任务，各两次，独立验收。对照是上一阶段已保存的六次运行，本次运行没有重跑对照；模型随机性与 provider 缓存未消除，不能将全部差值归因于定义缩短。

| 指标 | 前一阶段 | 本次 |
|---|---:|---:|
| 独立验收 | 6/6 | 6/6 |
| 有 usage 的模型轮次 | 76 | 71 |
| API input tokens | 478,855 | 380,852 |
| API uncached input | 107,143 | 85,940 |
| API output | 8,162 | 7,852 |
| 429 / 缺失 usage 请求 | 0 | 0 |
| Todo 独占轮次 | 11 | 20 |

本批 input 下降约 20.5%，uncached input 下降约 19.8%，轮次下降约 6.6%。Todo 独占轮次却增加，说明统一入口降低固定定义体积，但没有证明计划调用更高效。这是后续评估轮次行为的重点。

自管理另外执行真实只读用例 `current_profile`：CLI 命令成功，最终答复包含正确的 `eval.profile`。诊断轨迹显示先成功 `skill__get`，再成功 `manage_yourself`；技能结果约 18.4KB。按需加载将文档成本移到相关任务，自管理任务本身可能增加一轮和后续历史，不能承诺其整任务成本下降。

## 本地验证

- bot-core：串行测试 478 通过，9 忽略，暂跳过两项 rg 测试。
- 两项 rg 及相关第三项使用 `SHELL=/bin/bash` 单独复核，3/3 通过。默认 zsh 会加载本机 oh-my-zsh，输出只读缓存目录和缺失 docker completion 的报错，触发测试的 `No such file` 断言。没有修改用户 shell 配置。
- doctor 新旧 Todo 检测测试 1/1 通过。
- Linux debug 构建、Python 脚本语法检查、`git diff --check` 通过。

证据位于 `target/tool-surface-optimization-2026-10-31/`：`definitions.json`、`comparison.json`、`after/` 的完整请求与独立验收，以及 `self-management*.json` 和诊断轨迹。完整核心测试日志为 `target/tool-surface-tests.log`，rg 复核为 `target/tool-surface-rg-tests.log`。真实基准使用的冻结二进制在该目录 `after-remi-cat`；后续仅补充了回退提示一致性、Todo 展示和测试，最终源码另行构建通过。
