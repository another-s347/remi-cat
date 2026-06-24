use anyhow::Context;
use im_feishu::FeishuEventHookConfig;

use crate::config::{FeishuTransport, ImMode};

pub(crate) fn im_mode_from_env() -> ImMode {
    match std::env::var("REMI_IM_MODE")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("disabled" | "off" | "none") => ImMode::Disabled,
        _ => ImMode::Feishu,
    }
}

pub(super) fn feishu_transport_from_env() -> FeishuTransport {
    match std::env::var("REMI_FEISHU_TRANSPORT")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("event_hook" | "event-hook" | "hook" | "webhook") => FeishuTransport::EventHook,
        _ => FeishuTransport::WebSocket,
    }
}

pub(super) fn feishu_hook_config_from_env() -> anyhow::Result<FeishuEventHookConfig> {
    let host = std::env::var("REMI_FEISHU_HOOK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("REMI_FEISHU_HOOK_PORT")
        .unwrap_or_else(|_| "8788".to_string())
        .parse()
        .context("invalid REMI_FEISHU_HOOK_PORT")?;
    let path =
        std::env::var("REMI_FEISHU_HOOK_PATH").unwrap_or_else(|_| "/feishu/events".to_string());
    let verification_token = std::env::var("REMI_FEISHU_HOOK_VERIFICATION_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    Ok(FeishuEventHookConfig {
        addr: format!("{host}:{port}")
            .parse()
            .with_context(|| format!("invalid Feishu Event Hook address {host}:{port}"))?,
        path,
        verification_token,
    })
}
