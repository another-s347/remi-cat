use crate::channel::ChannelKind;

pub(crate) struct WebChannel;

impl WebChannel {
    pub(crate) fn kind(&self) -> ChannelKind {
        ChannelKind::Web
    }
}
