use anyhow::Context;
use serde::Deserialize;

use crate::model_profile::ModelProfileConfig;

#[derive(Debug, Clone, PartialEq)]
pub struct AccountUsage {
    pub provider: String,
    pub model_profile_id: String,
    pub model: String,
    pub status: AccountUsageStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountUsageStatus {
    Available { balances: Vec<AccountBalance> },
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountBalance {
    pub currency: Option<String>,
    pub total: Option<String>,
    pub granted: Option<String>,
    pub topped_up: Option<String>,
    pub voucher: Option<String>,
    pub cash: Option<String>,
    pub available: Option<bool>,
}

pub async fn query_account_usage(
    profile: &ModelProfileConfig,
    api_key: &str,
) -> anyhow::Result<AccountUsage> {
    let provider = infer_provider(profile);
    let status = match provider.as_str() {
        "deepseek" => query_deepseek_balance(profile, api_key).await?,
        "kimi" | "moonshot" => query_kimi_balance(profile, api_key).await?,
        _ => AccountUsageStatus::Unknown {
            reason: format!("provider `{provider}` does not expose a supported usage endpoint"),
        },
    };

    Ok(AccountUsage {
        provider,
        model_profile_id: profile.id.clone(),
        model: profile.model.clone(),
        status,
    })
}

fn infer_provider(profile: &ModelProfileConfig) -> String {
    if let Some(provider) = profile
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return provider.to_ascii_lowercase();
    }

    let model = profile.model.to_ascii_lowercase();
    let base_url = profile
        .base_url
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if model.contains("deepseek") || base_url.contains("deepseek.com") {
        "deepseek".to_string()
    } else if model.contains("mimo") || base_url.contains("xiaomimimo.com") {
        "mimo".to_string()
    } else if model.contains("kimi")
        || model.contains("moonshot")
        || base_url.contains("moonshot.cn")
        || base_url.contains("kimi.com")
    {
        "kimi".to_string()
    } else if model.contains("glm")
        || base_url.contains("bigmodel.cn")
        || base_url.contains("zhipu")
    {
        "glm".to_string()
    } else {
        "unknown".to_string()
    }
}

async fn query_deepseek_balance(
    profile: &ModelProfileConfig,
    api_key: &str,
) -> anyhow::Result<AccountUsageStatus> {
    let endpoint = format!(
        "{}/user/balance",
        base_url(profile, "https://api.deepseek.com").trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .get(endpoint)
        .bearer_auth(api_key)
        .send()
        .await
        .context("failed to query DeepSeek balance")?
        .error_for_status()
        .context("DeepSeek balance endpoint returned an error")?
        .json::<DeepSeekBalanceResponse>()
        .await
        .context("failed to parse DeepSeek balance response")?;

    Ok(AccountUsageStatus::Available {
        balances: response
            .balance_infos
            .into_iter()
            .map(|item| AccountBalance {
                currency: Some(item.currency),
                total: Some(item.total_balance),
                granted: Some(item.granted_balance),
                topped_up: Some(item.topped_up_balance),
                voucher: None,
                cash: None,
                available: Some(response.is_available),
            })
            .collect(),
    })
}

async fn query_kimi_balance(
    profile: &ModelProfileConfig,
    api_key: &str,
) -> anyhow::Result<AccountUsageStatus> {
    let endpoint = format!(
        "{}/users/me/balance",
        base_url(profile, "https://api.moonshot.cn/v1").trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .get(endpoint)
        .bearer_auth(api_key)
        .send()
        .await
        .context("failed to query Kimi balance")?
        .error_for_status()
        .context("Kimi balance endpoint returned an error")?
        .json::<KimiBalanceResponse>()
        .await
        .context("failed to parse Kimi balance response")?;

    Ok(AccountUsageStatus::Available {
        balances: vec![AccountBalance {
            currency: Some("CNY".to_string()),
            total: Some(trim_float(response.data.available_balance)),
            granted: None,
            topped_up: None,
            voucher: Some(trim_float(response.data.voucher_balance)),
            cash: Some(trim_float(response.data.cash_balance)),
            available: Some(response.status),
        }],
    })
}

fn base_url(profile: &ModelProfileConfig, fallback: &str) -> String {
    profile
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn trim_float(value: f64) -> String {
    let text = format!("{value:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[derive(Debug, Deserialize)]
struct DeepSeekBalanceResponse {
    is_available: bool,
    balance_infos: Vec<DeepSeekBalanceInfo>,
}

#[derive(Debug, Deserialize)]
struct DeepSeekBalanceInfo {
    currency: String,
    total_balance: String,
    granted_balance: String,
    topped_up_balance: String,
}

#[derive(Debug, Deserialize)]
struct KimiBalanceResponse {
    data: KimiBalanceData,
    status: bool,
}

#[derive(Debug, Deserialize)]
struct KimiBalanceData {
    available_balance: f64,
    voucher_balance: f64,
    cash_balance: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(provider: Option<&str>, model: &str, base_url: Option<&str>) -> ModelProfileConfig {
        ModelProfileConfig {
            id: "test".to_string(),
            name: "Test".to_string(),
            model: model.to_string(),
            base_url: base_url.map(ToOwned::to_owned),
            thinking: None,
            max_output_tokens: 1,
            context_tokens: 1,
            supports_images: false,
            short_term_tokens: 1,
            overflow_bytes: 1,
            auto_compress: true,
            description: None,
            provider: provider.map(ToOwned::to_owned),
            extra_options: serde_json::Map::new(),
        }
    }

    #[test]
    fn infers_supported_providers() {
        assert_eq!(
            infer_provider(&profile(Some("DeepSeek"), "x", None)),
            "deepseek"
        );
        assert_eq!(
            infer_provider(&profile(
                None,
                "kimi-k2.6",
                Some("https://api.moonshot.cn/v1")
            )),
            "kimi"
        );
        assert_eq!(
            infer_provider(&profile(None, "gpt-4o", Some("https://api.openai.com/v1"))),
            "unknown"
        );
        assert_eq!(
            infer_provider(&profile(
                None,
                "mimo-v2.5-pro",
                Some("https://api.xiaomimimo.com/v1")
            )),
            "mimo"
        );
        assert_eq!(
            infer_provider(&profile(
                None,
                "glm-5.2",
                Some("https://open.bigmodel.cn/api/paas/v4")
            )),
            "glm"
        );
    }

    #[test]
    fn trims_float_text() {
        assert_eq!(trim_float(49.588940), "49.58894");
        assert_eq!(trim_float(3.0), "3");
    }
}
