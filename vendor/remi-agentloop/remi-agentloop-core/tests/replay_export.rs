use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::{Stream, StreamExt};
use remi_agentloop_core::agent::Agent;
use remi_agentloop_core::builder::AgentBuilder;
use remi_agentloop_core::checkpoint::InMemoryCheckpointStore;
use remi_agentloop_core::context::{ContextStore, ContextStoreExt, InMemoryStore};
use remi_agentloop_core::error::AgentError;
use remi_agentloop_core::types::{AgentEvent, ChatRequest, ChatResponseChunk, Message, Role};
use serde_json::json;

#[derive(Clone)]
struct RecordingModel {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    responses: Arc<Mutex<VecDeque<Vec<ChatResponseChunk>>>>,
}

impl RecordingModel {
    fn new(responses: Vec<Vec<ChatResponseChunk>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Agent for RecordingModel {
    type Request = ChatRequest;
    type Response = ChatResponseChunk;
    type Error = AgentError;

    async fn chat(
        &self,
        req: Self::Request,
    ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
        self.requests.lock().unwrap().push(req);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![ChatResponseChunk::Done]);
        Ok(futures::stream::iter(response))
    }
}

async fn collect_events(stream: &mut (impl Stream<Item = AgentEvent> + Unpin)) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn fork_thread_at_index_copies_history_up_to_selected_message() {
    let store = InMemoryStore::new();
    let source = store.create_thread().await.unwrap();
    let messages = vec![
        Message::system("sys"),
        Message::user("hello"),
        Message::assistant("world"),
    ];
    store
        .append_messages(&source, messages.clone())
        .await
        .unwrap();

    let forked = store.fork_thread_at_index(&source, 1).await.unwrap();
    let forked_messages = store.get_messages(&forked).await.unwrap();

    assert_eq!(forked_messages.len(), 2);
    assert_eq!(forked_messages[0].role, Role::System);
    assert_eq!(forked_messages[1].role, Role::User);
    assert_ne!(forked_messages[0].id, messages[0].id);
    assert_ne!(forked_messages[1].id, messages[1].id);
}

#[tokio::test]
async fn export_thread_bundle_includes_latest_state_and_checkpoints() {
    let model = RecordingModel::new(vec![vec![
        ChatResponseChunk::Delta {
            content: "Hi there".into(),
            role: Some(Role::Assistant),
        },
        ChatResponseChunk::Done,
    ]]);
    let store = InMemoryStore::new();
    let checkpoints = InMemoryCheckpointStore::new();
    let agent = AgentBuilder::new()
        .model(model)
        .context_store(store)
        .checkpoint_store(checkpoints)
        .system("System prompt")
        .build();

    let thread_id = agent.create_thread().await.unwrap();
    let mut stream = agent.chat_in_thread(&thread_id, "Hello").await.unwrap();
    let _ = collect_events(&mut stream).await;

    let bundle = agent
        .export_thread_bundle(&thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bundle.version, 1);
    assert_eq!(bundle.thread_id, thread_id);
    assert!(!bundle.checkpoints.is_empty());
    assert_eq!(
        bundle.state.messages.last().unwrap().content.text_content(),
        "Hi there"
    );
    assert_eq!(
        bundle.replay.start_message_index,
        bundle.state.messages.len() - 1
    );
}

#[tokio::test]
async fn replay_from_message_index_creates_forked_thread_and_marks_user_state() {
    let model = RecordingModel::new(vec![
        vec![
            ChatResponseChunk::Delta {
                content: "Original reply".into(),
                role: Some(Role::Assistant),
            },
            ChatResponseChunk::Done,
        ],
        vec![
            ChatResponseChunk::Delta {
                content: "Replay reply".into(),
                role: Some(Role::Assistant),
            },
            ChatResponseChunk::Done,
        ],
    ]);
    let store = InMemoryStore::new();
    let checkpoints = InMemoryCheckpointStore::new();
    let agent = AgentBuilder::new()
        .model(model.clone())
        .context_store(store.clone())
        .checkpoint_store(checkpoints)
        .build();

    let thread_id = agent.create_thread().await.unwrap();
    let mut stream = agent.chat_in_thread(&thread_id, "Hello").await.unwrap();
    let _ = collect_events(&mut stream).await;

    let original_messages = store.get_messages(&thread_id).await.unwrap();
    assert!(original_messages.len() >= 2);

    let (replay_thread_id, mut replay_stream) = agent
        .replay_from_message_index(&thread_id, 0)
        .await
        .unwrap()
        .unwrap();
    let replay_events = collect_events(&mut replay_stream).await;

    assert_ne!(replay_thread_id, thread_id);
    assert!(replay_events
        .iter()
        .any(|event| matches!(event, AgentEvent::Done)));

    let replay_bundle = agent
        .export_thread_bundle(&replay_thread_id)
        .await
        .unwrap()
        .unwrap();
    let replay_meta = replay_bundle
        .state
        .user_state
        .get("replay")
        .cloned()
        .unwrap_or_else(|| json!(null));

    assert_eq!(
        replay_meta
            .get("source_thread_id")
            .and_then(|value| value.as_str()),
        Some(thread_id.0.as_str())
    );
    assert_eq!(
        replay_meta
            .get("start_message_index")
            .and_then(|value| value.as_u64()),
        Some(0)
    );

    let replay_messages = store.get_messages(&replay_thread_id).await.unwrap();
    assert!(replay_messages
        .iter()
        .any(|message| message.content.text_content() == "Replay reply"));
    assert_eq!(model.requests().len(), 2);
}
