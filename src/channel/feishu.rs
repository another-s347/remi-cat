use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use im_feishu::FeishuGateway;

use crate::app::run_feishu;
use crate::channel::{Channel, ChannelKind};
use crate::core::Runtime;

pub(crate) struct FeishuChannel {
    gateway: FeishuGateway,
}

impl FeishuChannel {
    pub(crate) fn new(gateway: FeishuGateway) -> Self {
        Self { gateway }
    }
}

impl Channel for FeishuChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Feishu
    }

    fn run<'a>(
        &'a self,
        runtime: Rc<Runtime>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        let gateway = self.gateway.clone();
        Box::pin(async move { run_feishu(runtime, gateway).await })
    }
}
