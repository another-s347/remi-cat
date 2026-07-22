use std::sync::Arc;

use remi_agentloop::prelude::Tool;
use remi_agentloop::tool::registry::DefaultToolRegistry;

use crate::im_tools::register_fetch_tool;
use crate::memory::{MemoryGetDetailTool, MemoryRecallTool, MemoryUpsertNamedTool};
use crate::search::SearchTool;
use crate::tool_tasks::ToolTasksTool;
use crate::tools::{
    ExaSearchTool, ManageYourselfTool, NowTool, RipgrepTool, RootedFsApplyPatchTool,
    RootedFsCreateTool, RootedFsLsTool, RootedFsReadTool, RootedFsRemoveTool, RootedFsWriteTool,
    SleepTool, WorkspaceBashTool, WorkspaceSshTool,
};
use crate::{skill, todo, AgentProfile, AskUserQuestionTool};

use super::LocalToolDeps;

pub(super) fn register_runtime_tools(
    local_tools: &mut DefaultToolRegistry,
    deps: &LocalToolDeps,
    agent_id: &str,
    include_user_question: bool,
) {
    macro_rules! register_builtin {
        ($name:literal, $tool:expr) => {
            if !deps
                .host_tools
                .iter()
                .any(|tool| tool.name() == $name && tool.allows_builtin_override())
            {
                local_tools.register($tool);
            }
        };
    }

    register_builtin!(
        "memory__get_detail",
        MemoryGetDetailTool {
            store: Arc::clone(&deps.memory),
            agent_id: agent_id.to_string(),
        }
    );
    register_builtin!(
        "memory__upsert_named",
        MemoryUpsertNamedTool {
            store: Arc::clone(&deps.memory),
            agent_id: agent_id.to_string(),
            workspace_root: deps.workspace_root.clone(),
        }
    );
    register_builtin!(
        "memory__recall",
        MemoryRecallTool {
            store: Arc::clone(&deps.memory),
            agent_id: agent_id.to_string(),
        }
    );
    register_builtin!(
        "search",
        SearchTool {
            skill_store: Arc::clone(&deps.skill_store),
            memory_store: Arc::clone(&deps.memory),
            agent_id: agent_id.to_string(),
        }
    );
    local_tools.register(ToolTasksTool::new(Arc::clone(&deps.tool_tasks)));
    let acp_support = deps
        .acp_client_tools
        .as_ref()
        .map(|(provider, support)| {
            provider.register_tools(local_tools, *support);
            *support
        })
        .unwrap_or_default();
    if deps.bash_enabled && !acp_support.terminal {
        local_tools.register(WorkspaceBashTool::new(
            Arc::clone(&deps.sandbox),
            Arc::clone(&deps.redactor),
        ));
    }
    local_tools.register(WorkspaceSshTool::new(Arc::clone(&deps.redactor)));
    if !acp_support.fs_read {
        local_tools.register(RootedFsReadTool {
            sandbox: Arc::clone(&deps.sandbox),
            redactor: Arc::clone(&deps.redactor),
        });
    }
    if !acp_support.fs_write {
        local_tools.register(RootedFsWriteTool {
            sandbox: Arc::clone(&deps.sandbox),
        });
    }
    local_tools.register(RootedFsApplyPatchTool {
        sandbox: Arc::clone(&deps.sandbox),
    });
    local_tools.register(RootedFsCreateTool {
        sandbox: Arc::clone(&deps.sandbox),
    });
    local_tools.register(RootedFsRemoveTool {
        sandbox: Arc::clone(&deps.sandbox),
    });
    local_tools.register(RootedFsLsTool {
        sandbox: Arc::clone(&deps.sandbox),
        redactor: Arc::clone(&deps.redactor),
    });
    local_tools.register(RipgrepTool {
        sandbox: Arc::clone(&deps.sandbox),
        redactor: Arc::clone(&deps.redactor),
    });
    register_fetch_tool(
        local_tools,
        deps.workspace_root.clone(),
        deps.sandbox.workspace_root_label(),
        deps.sandbox.kind() != "docker",
        deps.im_bridge.clone(),
    );
    local_tools.register(ExaSearchTool::new());
    local_tools.register(NowTool);
    local_tools.register(SleepTool);
    local_tools.register(ManageYourselfTool);
    if include_user_question {
        local_tools.register(AskUserQuestionTool::new(Arc::clone(
            &deps.user_question_manager,
        )));
    }
}

pub(super) fn build_subagent_tools(
    deps: &LocalToolDeps,
    profile: &AgentProfile,
) -> DefaultToolRegistry {
    let mut local_tools = DefaultToolRegistry::new();
    skill::register_skill_tools(&mut local_tools, Arc::clone(&deps.skill_store));
    todo::register_todo_tools(&mut local_tools, Arc::clone(&deps.todo_backend));
    register_runtime_tools(&mut local_tools, deps, &profile.id, true);
    for tool in &deps.host_tools {
        local_tools.register(tool.clone());
    }
    local_tools
}
