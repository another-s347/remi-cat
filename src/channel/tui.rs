use crate::app::CliConfig;
use crate::application::Application;
use crate::channel::ChannelKind;

pub(crate) struct TuiChannel {
    config: CliConfig,
}

impl TuiChannel {
    pub(crate) fn new(config: CliConfig) -> Self {
        Self { config }
    }
}

impl TuiChannel {
    #[allow(dead_code)]
    fn kind(&self) -> ChannelKind {
        ChannelKind::Tui
    }
    pub(crate) async fn run_once(self, application: Application) -> anyhow::Result<()> {
        crate::tui_app::run_tui(application, self.config).await
    }
}
