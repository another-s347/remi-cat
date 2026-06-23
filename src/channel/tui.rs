use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::app::CliConfig;
use crate::channel::{Channel, ChannelKind};
use crate::core::Runtime;
use crate::local_trigger_scheduler::LocalTriggerDispatch;

pub(crate) struct TuiChannel {
    config: CliConfig,
    trigger_rx: Option<tokio::sync::mpsc::UnboundedReceiver<LocalTriggerDispatch>>,
}

impl TuiChannel {
    pub(crate) fn new(
        config: CliConfig,
        trigger_rx: Option<tokio::sync::mpsc::UnboundedReceiver<LocalTriggerDispatch>>,
    ) -> Self {
        Self { config, trigger_rx }
    }
}

impl Channel for TuiChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Tui
    }

    fn run<'a>(
        &'a self,
        _runtime: Rc<Runtime>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        Box::pin(async {
            anyhow::bail!(
                "TuiChannel owns a one-shot trigger receiver and must be run through run_once"
            )
        })
    }
}

impl TuiChannel {
    pub(crate) async fn run_once(self, runtime: Rc<Runtime>) -> anyhow::Result<()> {
        crate::tui_app::run_tui(runtime, self.config, self.trigger_rx).await
    }
}
