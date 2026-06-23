use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::app::{run_cli, CliConfig};
use crate::channel::{Channel, ChannelKind};
use crate::core::Runtime;

pub(crate) struct CliChannel {
    config: CliConfig,
}

impl CliChannel {
    pub(crate) fn new(config: CliConfig) -> Self {
        Self { config }
    }
}

impl Channel for CliChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Cli
    }

    fn run<'a>(
        &'a self,
        runtime: Rc<Runtime>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        let config = self.config.clone();
        Box::pin(async move { run_cli(runtime, config).await })
    }
}
