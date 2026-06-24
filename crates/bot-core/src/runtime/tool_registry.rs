use std::sync::Arc;

use remi_agentloop::tool::registry::DefaultToolRegistry;

use crate::im_tools::register_fetch_tool;
use crate::memory::{MemoryGetDetailTool, MemoryRecallTool, MemoryUpsertNamedTool};
use crate::search::SearchTool;
use crate::tools::{
    ExaSearchTool, ManageYourselfTool, NowTool, RootedFsApplyPatchTool, RootedFsCreateTool,
    RootedFsLsTool, RootedFsReadTool, RootedFsRemoveTool, RootedFsWriteTool, SleepTool,
    WorkspaceBashTool, WorkspaceSshTool,
};
use crate::{skill, todo, trigger, AgentProfile, AskUserQuestionTool};

use super::LocalToolDeps;

pub(super) fn register_runtime_tools(
    local_tools: &mut DefaultToolRegistry,
    deps: &LocalToolDeps,
    agent_id: &str,
    include_user_question: bool,
) {
    local_tools.register(MemoryGetDetailTool {
        store: Arc::clone(&deps.memory),
        agent_id: agent_id.to_string(),
    });
    local_tools.register(MemoryUpsertNamedTool {
        store: Arc::clone(&deps.memory),
        agent_id: agent_id.to_string(),
        workspace_root: deps.workspace_root.clone(),
    });
    local_tools.register(MemoryRecallTool {
        store: Arc::clone(&deps.memory),
        agent_id: agent_id.to_string(),
    });
    local_tools.register(SearchTool {
        skill_store: Arc::clone(&deps.skill_store),
        memory_store: Arc::clone(&deps.memory),
        agent_id: agent_id.to_string(),
    });
    if deps.bash_enabled {
        local_tools.register(WorkspaceBashTool::new(
            Arc::clone(&deps.sandbox),
            Arc::clone(&deps.redactor),
        ));
    }
    local_tools.register(WorkspaceSshTool::new(Arc::clone(&deps.redactor)));
    local_tools.register(RootedFsReadTool {
        sandbox: Arc::clone(&deps.sandbox),
        redactor: Arc::clone(&deps.redactor),
    });
    local_tools.register(RootedFsWriteTool {
        sandbox: Arc::clone(&deps.sandbox),
    });
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
    trigger::register_trigger_tools(&mut local_tools, Arc::clone(&deps.trigger_backend));
    register_runtime_tools(&mut local_tools, deps, &profile.id, false);
    local_tools
}
