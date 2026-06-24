use im_feishu::FeishuMessage;

pub(crate) fn feishu_session_channel_id(msg: &FeishuMessage) -> String {
    let thread_id = msg
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if msg.chat_type == "group" {
        if let Some(thread_id) = thread_id {
            return feishu_topic_channel_id(&msg.chat_id, thread_id);
        }
    }
    msg.chat_id.clone()
}

pub(crate) fn feishu_topic_channel_id(chat_id: &str, thread_id: &str) -> String {
    format!("{}:thread:{}", chat_id.trim(), thread_id.trim())
}

pub(crate) fn should_ignore_unaddressed_topic_start(
    msg: &FeishuMessage,
    session_exists: bool,
) -> bool {
    msg.chat_type == "group"
        && msg
            .thread_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        && !session_exists
        && !msg.at_bot
}
