# 原始 response 横向复核（2026-11-01）

范围：沿用完整长任务批次的18次默认运行和6次Remi等待运行，无新模型调用。读取逐请求SSE response与随后sent.json，排除标题请求；完整响应以收到finish_reason为界，未完成尾部不计入表格。thinking均关闭，无法观察未输出的内部推理。

| 模式 | 完整主响应 | 含工具调用 | 工具调用同时含正文 | 非空reasoning_content |
|---|---:|---:|---:|---:|
| dsh |176|170|6|0|
| OpenCode |219|215|0|0|
| Remi默认 |89|86|13|0|
| Remi等待 |288|284|14|0|

因此“没有自然语言解释”不是异常的充分条件，OpenCode的215条完整工具响应均没有附带正文；Remi默认反而更常输出正文。本表不是质量排名，不同运行时长和失败轨迹不可当作等量样本。

## Rust任务中的失败恢复

- dsh-trailindex-1：012执行cargo test，013补PathBuf导入，014重测，015读scan.rs，016正文指出仅PID命名临时目录导致并发碰撞并修改。027集成测试失败后，028正文发现AtomicU64::new(0)每次重新创建导致计数恒零，修改后029重测。
- dsh-trailindex-2：024单测通过；025一次命令执行三轮全量测试，失败数变化；026改为单线程诊断。032最小复现产生ENOENT；033在复现中创建缺少的父目录，034修改tests/basic.rs。该ENOENT来自不同代码位置，不能等同于Remi的current_dir错误。
- opencode-trailindex-1：007编译失败，008读store.rs，009修括号；017测试失败，018读测试，019修改。转义问题曾多次修错并反复测试，但021重写测试、024读代码、028 execute、035最小Rust探针、036读测试、037再修改，持续改变诊断动作，043最终回复。不能描述成一次修好或始终高效。
- opencode-trailindex-2：012测试后013-015修改，016重测，017修改，018重测，019修改临时目录生成，020重测，021清理导入，022重测，023最终回复。
- waiting/remi-trailindex-1：014离线测试，015在线测试，016which cargo，017读测试，018改测试，019重测，020运行CLI，021启用backtrace，022读233-290行（覆盖实际问题），023编译，024全量测试，025带nocapture单测，026起收敛到同参exact单测。026-141连续116次，没有正文、读文件或修改，结束原因为tool_calls。整次141个response均没有非空正文。循环前曾有有效诊断动作，不能说一开始就只会重跑。
- waiting/remi-trailindex-2：同样几乎只输出工具调用，但失败后会读scan.rs、检查临时目录和符号链接、编写最小Rust探针、修改metadata逻辑并重测，036最终回复。因此Remi的纯工具响应也能完成恢复。

## 工具结果与历史传递

Remi循环中的下一轮输入完整包含tests/basic.rs:247:10、NotFound、FAILED及[exit 101]，关键日志未被截断，也没有后台task_id；每次原始SSE均生成新的工具调用。OpenCode的非零退出显示Exited with code 101，dsh显示[exit code: 101]，Remi显示[exit 101]。失败信号均明确，未观察到能解释116次循环的错误信息缺失。

检查随后保存输入中出现的工具调用：dsh214条、OpenCode274条、Remi默认143条、Remi等待336条。比较工具名、JSON解析后的参数，以及关联assistant正文，均无语义差异。dsh/OpenCode对参数JSON的空白格式会重序列化，原字符串不等不代表参数丢失。未回传到下一请求的终止响应不在此检查范围。

## 工具定义差异

dsh的bash强制description与command，并要求先生成description。实际能看到Run tests single-threaded to diagnose race conditions等意图；OpenCode shell与Remi bash仅强制command。description可能是低成本的显式意图约束，但无因果证据：OpenCode没有它也能排错，dsh也会生成笼统的Run cargo test offline again。

## 判断

可确认的差异是失败后的动作选择：Remi异常轨迹从诊断转为无进展重跑，而其他Rust轨迹持续插入读取、修改或有目的的实验。不是SSE格式不同、隐藏reasoning输出不同，也未发现Remi把响应丢失后误回放的证据。仍不足以定位哪一条prompt导致循环。应先增加停滞检测，再用相同故障起点对比恢复；单独加入description或恢复提示需要控制变量验证。

机器可读证据：target/token-extended-comparison-2026-10-31/response-comparison/，包含24份完整提取轨迹、summary.json与history-audit.json。summary.json的history_differences是原始字符串比较，仅用于诊断格式差异；语义检查以history-audit.json为准。
