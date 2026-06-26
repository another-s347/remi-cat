//! Core runtime boundary.
//!
//! Channel implementations convert native inputs into chat requests and drive
//! the runtime through this module. Concrete transports stay outside this core
//! state.

mod chat;
mod sub_session;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use bot_core::im_tools::ImFileBridge;
use bot_core::CatBot;
use tokio::sync::Mutex;
use user_store::UserStore;

use crate::secret_store::SecretStore;
use crate::session::SessionRuntime;

pub(crate) use chat::{ChatChannel, ChatRequest, CoreChatEvent};
pub(crate) use sub_session::{
    append_direct_sub_session_turn, sub_session_input_target, SubSessionInputTarget,
};

pub(crate) struct Runtime {
    pub(crate) bot: Rc<CatBot>,
    pub(crate) secret_store: Arc<Mutex<SecretStore>>,
    pub(crate) user_store: Arc<UserStore>,
    pub(crate) sessions: Arc<Mutex<SessionRuntime>>,
    pub(crate) im_bridge: Arc<dyn ImFileBridge>,
    pub(crate) root_agent_id: String,
    pub(crate) data_dir: PathBuf,
}
