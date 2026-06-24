//! Supervisor-agent workflow building blocks.
//!
//! The supervisor layer is separated from the markdown runtime so applications
//! can compose goal/workflow supervision independently from the channel that
//! delivers messages.

pub use crate::goal::{GoalMaxRounds, GoalState, GoalStatus, SupervisorDecision};
pub use crate::supervisor_workflow::{
    apply_decision, clear_instance, embedded_goal_definition, instance_from_user_state,
    list_definitions, load_definition, parse_decision, remove_instance_from_user_state,
    set_instance_in_user_state, supervisor_prompt, WorkflowDecision, WorkflowDefinition,
    WorkflowEdge, WorkflowInstance, WorkflowMaxRounds, WorkflowNode, WorkflowReport,
    WorkflowStatus,
};
