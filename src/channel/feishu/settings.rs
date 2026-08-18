use anyhow::Context;
use im_feishu::FeishuEventHookConfig;

pub(super) fn feishu_hook_config(
    config: &crate::runtime_config::FeishuEventHookRuntimeConfig,
) -> anyhow::Result<FeishuEventHookConfig> {
    Ok(FeishuEventHookConfig {
        addr: format!("{}:{}", config.host, config.port)
            .parse()
            .with_context(|| {
                format!(
                    "invalid Feishu Event Hook address {}:{}",
                    config.host, config.port
                )
            })?,
        path: config.path.clone(),
        verification_token: (!config.verification_token.trim().is_empty())
            .then(|| config.verification_token.clone()),
    })
}
