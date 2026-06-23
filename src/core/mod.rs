//! Core runtime boundary.
//!
//! Channel implementations convert native inputs into shared channel events and
//! drive the runtime through this module. Concrete transports stay outside this
//! core state.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use bot_core::im_tools::ImFileBridge;
use bot_core::CatBot;
use tokio::sync::Mutex;
use tracing::warn;
use user_store::UserStore;

use crate::secret_store::SecretStore;
use crate::session::SessionRuntime;

pub(crate) struct Runtime {
    pub(crate) bot: Rc<CatBot>,
    pub(crate) secret_store: Arc<Mutex<SecretStore>>,
    pub(crate) user_store: Arc<UserStore>,
    pub(crate) sessions: Arc<Mutex<SessionRuntime>>,
    pub(crate) im_bridge: Arc<dyn ImFileBridge>,
    pub(crate) root_agent_id: String,
    pub(crate) data_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubSessionInputTarget {
    Acp {
        acp_session_id: String,
        sub_thread_id: String,
    },
    Agent {
        sub_thread_id: String,
        agent_name: String,
    },
}

pub(crate) async fn sub_session_input_target(
    runtime: &Runtime,
    session_id: &str,
) -> Option<SubSessionInputTarget> {
    let (sub_thread_id, agent_name, acp_session_id) = {
        let sessions = runtime.sessions.lock().await;
        let sub_thread_id = sessions.metadata_string(session_id, "sub_session_thread_id")?;
        let agent_name = sessions
            .metadata_string(session_id, "sub_session_agent")
            .unwrap_or_default();
        let acp_session_id = sessions.metadata_string(session_id, "sub_session_acp_session_id");
        (sub_thread_id, agent_name, acp_session_id)
    };
    if agent_name == "acp" {
        let acp_session_id = match acp_session_id {
            Some(id) if !id.trim().is_empty() => id,
            _ => {
                let id = runtime
                    .bot
                    .acp_session_id_for_sub_session(&sub_thread_id)
                    .await?;
                let _ = runtime.sessions.lock().await.set_metadata_string(
                    session_id,
                    "sub_session_acp_session_id",
                    &id,
                );
                id
            }
        };
        return Some(SubSessionInputTarget::Acp {
            acp_session_id,
            sub_thread_id,
        });
    }
    if agent_name.trim().is_empty() {
        return None;
    }
    Some(SubSessionInputTarget::Agent {
        sub_thread_id,
        agent_name,
    })
}

pub(crate) async fn append_direct_sub_session_turn(
    runtime: &Runtime,
    session_id: &str,
    user_text: &str,
    assistant_text: &str,
) {
    if let Err(err) = runtime
        .bot
        .append_thread_messages(
            session_id,
            vec![
                remi_agentloop::prelude::Message::user(user_text.to_string()),
                remi_agentloop::prelude::Message::assistant(assistant_text.to_string()),
            ],
        )
        .await
    {
        warn!("failed to append direct sub-session turn: {err:#}");
    }
}
