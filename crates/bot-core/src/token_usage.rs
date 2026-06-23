use remi_agentloop::prelude::Message;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub max_prompt_tokens: u32,
}

impl TokenUsage {
    pub fn total_tokens(self) -> u32 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ContextMetrics {
    pub used_tokens: u32,
    pub context_tokens: u32,
    pub ratio: f64,
    pub percent: f64,
}

impl ContextMetrics {
    pub fn from_usage(usage: TokenUsage, context_tokens: u32) -> Self {
        let used_tokens = usage.max_prompt_tokens;
        let ratio = context_usage_ratio(used_tokens, context_tokens);
        Self {
            used_tokens,
            context_tokens,
            ratio,
            percent: ratio * 100.0,
        }
    }

    pub fn rounded_percent(self) -> Option<u32> {
        if self.context_tokens == 0 {
            return None;
        }
        Some(self.percent.round() as u32)
    }
}

pub fn context_usage_ratio(used_tokens: u32, context_tokens: u32) -> f64 {
    if context_tokens == 0 {
        return 0.0;
    }
    let used = used_tokens.min(context_tokens);
    used as f64 / context_tokens as f64
}

pub fn context_budget_tokens(context_tokens: u32, percent: usize) -> usize {
    (context_tokens as usize)
        .saturating_mul(percent)
        .div_ceil(100)
        .max(1)
}

pub fn estimate_model_input_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let units = text.chars().fold(0_f64, |sum, ch| {
        sum + if ch.is_ascii() { 0.25 } else { 1.0 }
    });
    units.ceil().max(1.0) as u32
}

pub fn estimate_memory_message_tokens(msgs: &[Message]) -> usize {
    msgs.iter()
        .map(|m| m.content.text_content().len() / 4 + 10)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_metrics_use_max_prompt_tokens_and_clamp() {
        let usage = TokenUsage {
            prompt_tokens: 260_000,
            completion_tokens: 123,
            max_prompt_tokens: 130_000,
        };
        let metrics = ContextMetrics::from_usage(usage, 128_000);
        assert_eq!(metrics.used_tokens, 130_000);
        assert_eq!(metrics.context_tokens, 128_000);
        assert_eq!(metrics.rounded_percent(), Some(100));
        assert_eq!(metrics.ratio, 1.0);
    }

    #[test]
    fn context_metrics_returns_none_percent_without_context_window() {
        let metrics = ContextMetrics::from_usage(
            TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 0,
                max_prompt_tokens: 1,
            },
            0,
        );
        assert_eq!(metrics.rounded_percent(), None);
        assert_eq!(metrics.ratio, 0.0);
    }

    #[test]
    fn model_input_token_estimate_handles_ascii_and_cjk() {
        assert_eq!(estimate_model_input_tokens(""), 0);
        assert_eq!(estimate_model_input_tokens("abcd"), 1);
        assert_eq!(estimate_model_input_tokens("abcde"), 2);
        assert_eq!(estimate_model_input_tokens("你好"), 2);
        assert_eq!(estimate_model_input_tokens("hi你"), 2);
    }

    #[test]
    fn context_budget_tokens_rounds_up_and_never_returns_zero() {
        assert_eq!(context_budget_tokens(128_000, 80), 102_400);
        assert_eq!(context_budget_tokens(1, 80), 1);
    }
}
