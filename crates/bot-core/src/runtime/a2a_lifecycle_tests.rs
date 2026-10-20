use super::*;
use crate::a2a_delegate::{A2aDelegateTransport, TASK_NOT_ACTIVE};
use a2a::{
    Artifact, Part, StreamResponse, TaskArtifactUpdateEvent, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};
use std::sync::Mutex as StdMutex;

struct LifecycleTransport {
    requests: StdMutex<Vec<a2a::SendMessageRequest>>,
    held: StdMutex<Vec<tokio::sync::mpsc::Sender<Result<StreamResponse, String>>>>,
    cancelled: tokio::sync::Notify,
    hold_first: bool,
    steer_error: &'static str,
}
impl LifecycleTransport {
    fn new(hold_first: bool, steer_error: &'static str) -> Arc<Self> {
        Arc::new(Self {
            requests: StdMutex::new(vec![]),
            held: StdMutex::new(vec![]),
            cancelled: tokio::sync::Notify::new(),
            hold_first,
            steer_error,
        })
    }
}
#[async_trait::async_trait]
impl A2aDelegateTransport for LifecycleTransport {
    async fn invoke(
        &self,
        request: a2a::SendMessageRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamResponse, String>>, String> {
        let (number, context) = {
            let mut requests = self.requests.lock().unwrap();
            let context = request.message.context_id.clone().unwrap();
            requests.push(request);
            (requests.len(), context)
        };
        let id = format!("task-{number}");
        let status = |state| {
            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: id.clone(),
                context_id: context.clone(),
                status: TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })
        };
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(Ok(status(TaskState::Working))).await.unwrap();
        tx.send(Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: id.clone(), context_id: context.clone(),
            artifact: Artifact { artifact_id: crate::a2a_delegate::ACTIVITY_ARTIFACT_ID.into(), name: None, description: None,
                parts: vec![Part::data(serde_json::json!({"event":"text_delta","data":{"text":format!("result {number}")}}))], metadata: None, extensions: None },
            append: Some(false), last_chunk: Some(false), metadata: None,
        }))).await.unwrap();
        if self.hold_first && number == 1 {
            self.held.lock().unwrap().push(tx);
        } else {
            tx.send(Ok(status(TaskState::Completed))).await.unwrap();
        }
        Ok(rx)
    }
    async fn cancel(&self, _task: &str) -> Result<(), String> {
        self.held.lock().unwrap().clear();
        self.cancelled.notify_one();
        Ok(())
    }
    async fn steer(&self, _task: &str, _content: Content) -> Result<(), String> {
        Err(self.steer_error.into())
    }
    async fn decide_approval(
        &self,
        _task: &str,
        _approval: &str,
        _decision: crate::ToolApprovalDecision,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn answer_question(
        &self,
        _task: &str,
        _question: &str,
        _response: crate::UserQuestionResponse,
    ) -> Result<(), String> {
        Ok(())
    }
}
fn tool(dir: &std::path::Path, transport: Arc<LifecycleTransport>) -> RemiSubAgentTool {
    RemiSubAgentTool {
        name: "agent__worker".into(),
        description: "test".into(),
        parameters_schema: serde_json::json!({}),
        agent_name: "worker".into(),
        sessions: Arc::new(SubagentSessionCoordinator::default()),
        approval_manager: ToolApprovalManager::new(),
        user_question_manager: UserQuestionManager::new(),
        hook_manager: HookManager::with_sources(dir.into(), dir.into(), false, vec![]).unwrap(),
        workspace_root: dir.into(),
        model_name: "test".into(),
        a2a_delegate_transport: Some(transport),
    }
}
async fn finish(tool: &RemiSubAgentTool, ctx: ToolContext) -> serde_json::Value {
    let ToolResult::Output(mut output) = tool
        .execute_a2a("continue".into(), "inspect_project".into(), None, ctx)
        .await
        .unwrap()
    else {
        panic!("expected stream")
    };
    let mut result = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = output.next().await {
            if let ToolOutput::Result(Content::Text(text)) = event {
                result = Some(serde_json::from_str(&text).unwrap());
            }
        }
    })
    .await
    .unwrap();
    result.unwrap()
}

#[tokio::test]
async fn dropped_a2a_tool_cleans_binding_cancels_child_and_reuses_named_session() {
    let dir = tempfile::tempdir().unwrap();
    let transport = LifecycleTransport::new(true, TASK_NOT_ACTIVE);
    let tool = tool(dir.path(), Arc::clone(&transport));
    let ctx = ToolContext::default();
    let parent_cancel = ctx.runtime().cancellation();
    let ToolResult::Output(mut output) = tool
        .execute_a2a(
            "inspect".into(),
            "inspect_project".into(),
            None,
            ctx.clone(),
        )
        .await
        .unwrap()
    else {
        panic!("expected stream")
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while tool
            .sessions
            .remote("subagent:worker:inspect_project")
            .is_none()
        {
            assert!(output.next().await.is_some());
        }
    })
    .await
    .unwrap();
    drop(output);
    assert!(tool
        .sessions
        .remote("subagent:worker:inspect_project")
        .is_none());
    assert!(!tool.sessions.cancel("subagent:worker:inspect_project"));
    assert!(
        !parent_cancel.is_cancelled(),
        "child completion must not cancel its parent"
    );
    tokio::time::timeout(Duration::from_secs(5), transport.cancelled.notified())
        .await
        .unwrap();
    let result = finish(&tool, ctx).await;
    assert_eq!(result["success"], true);
    assert_eq!(result["final_output"], "result 2");
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].message.context_id,
        requests[1].message.context_id
    );
}

#[tokio::test]
async fn inactive_a2a_task_is_replaced_but_other_steer_errors_are_not_replayed() {
    let dir = tempfile::tempdir().unwrap();
    for (error, succeeds) in [
        (TASK_NOT_ACTIVE, true),
        ("A2A transport failed", false),
        ("unauthorized", false),
    ] {
        let transport = LifecycleTransport::new(false, error);
        let tool = tool(dir.path(), Arc::clone(&transport));
        let client =
            crate::a2a_delegate::A2aDelegateClient::from_env("worker", Some(transport.clone()))
                .unwrap();
        tool.sessions.bind_remote(
            "subagent:worker:inspect_project",
            client,
            "expired-task".into(),
        );
        if succeeds {
            let result = finish(&tool, ToolContext::default()).await;
            assert_eq!(result["success"], true);
            assert_eq!(result["final_output"], "result 1");
            assert_eq!(transport.requests.lock().unwrap().len(), 1);
        } else {
            assert!(tool
                .execute_a2a(
                    "continue".into(),
                    "inspect_project".into(),
                    None,
                    ToolContext::default()
                )
                .await
                .is_err());
            assert!(transport.requests.lock().unwrap().is_empty());
        }
    }
}
