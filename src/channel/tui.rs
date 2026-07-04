use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::app::CliConfig;
use crate::channel::{Channel, ChannelKind};
use crate::core::Runtime;

pub(crate) struct TuiChannel {
    config: CliConfig,
}

impl TuiChannel {
    pub(crate) fn new(config: CliConfig) -> Self {
        Self { config }
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
        Box::pin(async { anyhow::bail!("TuiChannel must be run through run_once") })
    }
}

impl TuiChannel {
    pub(crate) async fn run_once(self, runtime: Rc<Runtime>) -> anyhow::Result<()> {
        crate::tui_app::run_tui(runtime, self.config).await
    }
}
