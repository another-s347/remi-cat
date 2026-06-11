export type ChatCommand = {
  value: string;
  label: string;
  description: string;
  keywords?: string[];
  acceptsArguments?: boolean;
};

export const CHAT_COMMANDS: ChatCommand[] = [
  {
    value: "/tools",
    label: "列出工具",
    description: "显示当前 Agent 可用的工具",
    keywords: ["tool", "工具"],
  },
  {
    value: "/goal status",
    label: "查看 Goal",
    description: "查看当前会话的目标与监督状态",
    keywords: ["goal", "目标", "status"],
  },
  {
    value: "/goal set ",
    label: "设置 Goal",
    description: "设置目标；可选 --max-rounds N|unlimited",
    keywords: ["goal", "目标", "set"],
    acceptsArguments: true,
  },
  {
    value: "/goal clear",
    label: "清除 Goal",
    description: "清除当前会话的目标",
    keywords: ["goal", "目标", "clear"],
  },
  {
    value: "/workflow status",
    label: "查看 Workflow",
    description: "查看当前 supervisor workflow 状态",
    keywords: ["workflow", "工作流", "status"],
  },
  {
    value: "/workflow start ",
    label: "启动 Workflow",
    description: "启动工作流；填写 ID 和可选参数",
    keywords: ["workflow", "工作流", "start"],
    acceptsArguments: true,
  },
  {
    value: "/workflow stop",
    label: "停止 Workflow",
    description: "停止当前 supervisor workflow",
    keywords: ["workflow", "工作流", "stop"],
  },
  {
    value: "/compact",
    label: "压缩记忆",
    description: "立即将短期记忆压缩为中期摘要",
    keywords: ["compact", "memory", "压缩", "记忆"],
  },
  {
    value: "/clear",
    label: "清空历史",
    description: "清空当前会话历史，保留 Todo 和 Trigger 状态",
    keywords: ["clear", "history", "清空", "历史"],
  },
  {
    value: "/doctor",
    label: "运行诊断",
    description: "显示 Remi 运行环境与配置诊断",
    keywords: ["doctor", "health", "诊断"],
  },
  {
    value: "/usage",
    label: "查询额度",
    description: "查询当前模型 API 账户额度",
    keywords: ["usage", "balance", "quota", "用量", "额度", "余额"],
  },
  {
    value: "/model status",
    label: "模型状态",
    description: "显示当前 session 的模型配置状态",
    keywords: ["model", "模型", "状态", "status"],
  },
  {
    value: "/model list",
    label: "模型列表",
    description: "列出可用模型 profile",
    keywords: ["model", "models", "模型", "列表"],
  },
  {
    value: "/model use ",
    label: "切换模型",
    description: "切换当前 session 的模型 profile",
    keywords: ["model", "use", "switch", "模型", "切换"],
    acceptsArguments: true,
  },
  {
    value: "/model reset",
    label: "重置模型",
    description: "清除当前 session 的模型 override",
    keywords: ["model", "reset", "模型", "重置"],
  },
  {
    value: "/skill list",
    label: "Skill 列表",
    description: "列出可用 skills",
    keywords: ["skill", "技能", "列表"],
  },
  {
    value: "/skill status",
    label: "Skill 状态",
    description: "显示当前 session 已读取的 skills",
    keywords: ["skill", "技能", "状态"],
  },
];

export function commandSuggestions(input: string, commands: ChatCommand[] = CHAT_COMMANDS): ChatCommand[] {
  const normalized = input.trimStart().toLocaleLowerCase();
  if (!normalized.startsWith("/") || normalized.includes("\n")) return [];
  const terms = normalized.slice(1).split(/\s+/).filter(Boolean);
  return commands.filter((command) => {
    const searchable = [command.value.slice(1), command.label, command.description, ...(command.keywords ?? [])]
      .join(" ")
      .toLocaleLowerCase();
    return terms.every((term) => searchable.includes(term));
  });
}
