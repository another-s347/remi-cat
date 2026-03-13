//! Built-in model performance profiles.
//!
//! [`ModelProfile`] records the key capability limits of each supported model.
//! Unknown models fall back to [`ModelProfile::FALLBACK`].
//!
//! Derived configuration helpers:
//! - [`ModelProfile::default_short_term_tokens`] — memory compression budget
//! - [`ModelProfile::default_overflow_bytes`] — max inline tool-output bytes

/// Performance profile for a single LLM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProfile {
    /// Model name (or prefix pattern that matches this profile).
    pub name: &'static str,
    /// Maximum context window in tokens (input + output combined).
    pub context_tokens: u32,
    /// Maximum generation / output tokens.
    pub max_output_tokens: u32,
    /// Default OpenAI-compatible API base URL for this model's provider.
    /// `None` for models that have no single canonical endpoint (e.g. Llama)
    /// or where the endpoint requires non-standard auth headers (e.g. Claude).
    pub base_url: Option<&'static str>,
}

impl ModelProfile {
    /// Fallback profile used when no known model matches.
    /// Assumes a conservative 32 k-token context.
    pub const FALLBACK: ModelProfile = ModelProfile {
        name: "<unknown>",
        context_tokens: 32_768,
        max_output_tokens: 4_096,
        base_url: None,
    };

    // ── Derived configuration ─────────────────────────────────────────────

    /// Recommended short-term memory token budget.
    ///
    /// Targets ≈ 25 % of the context window to leave headroom for the system
    /// prompt, tool definitions, and the model's own output.
    /// Clamped to [4 000, 64 000].
    pub fn default_short_term_tokens(&self) -> usize {
        let raw = self.context_tokens as usize / 4;
        raw.clamp(4_000, 64_000)
    }

    /// Recommended maximum byte size for inline tool-output content.
    ///
    /// Outputs exceeding this threshold are spilled to a temp file so the
    /// agent can read them in chunks via `fs_read`.
    ///
    /// Targets ≈ 40 % of `default_short_term_tokens` × 4 bytes/token,
    /// i.e. ~10 % of the context in characters.
    /// Clamped to [8 192, 200 000].
    pub fn default_overflow_bytes(&self) -> usize {
        let raw = self.default_short_term_tokens() * 4 * 40 / 100;
        raw.clamp(8_192, 200_000)
    }

    /// Default OpenAI-compatible API base URL for this model's provider.
    ///
    /// Returns `None` for models without a canonical single endpoint
    /// (e.g. open-source Llama hosted on many different providers) or where
    /// the provider requires non-standard auth handling (e.g. Anthropic).
    pub fn default_base_url(&self) -> Option<&'static str> {
        self.base_url
    }

    // ── Lookup ────────────────────────────────────────────────────────────

    /// Find the profile for a given model name.
    ///
    /// Matching is done by checking whether the lowercased model name
    /// *contains* any of the registered prefix patterns (most-specific first).
    /// Falls back to [`ModelProfile::FALLBACK`] for unknown models.
    pub fn for_model(model: &str) -> &'static ModelProfile {
        let lower = model.to_lowercase();
        PROFILES
            .iter()
            .find(|p| lower.contains(p.name))
            .unwrap_or(&ModelProfile::FALLBACK)
    }
}

// ── Base URL constants ────────────────────────────────────────────────────────

const OPENAI_URL: &str = "https://api.openai.com/v1";
const DEEPSEEK_URL: &str = "https://api.deepseek.com/v1";
const DASHSCOPE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const GEMINI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
const MISTRAL_URL: &str = "https://api.mistral.ai/v1";
const MOONSHOT_URL: &str = "https://api.moonshot.cn/v1";

// ── Static profile table ──────────────────────────────────────────────────────
//
// Rules:
//  • List more-specific entries BEFORE less-specific ones (first match wins).
//  • All `name` values must be lowercase.
//  • `context_tokens` is the *total* context (input + output) in tokens.
//  • `base_url` is the OpenAI-compatible endpoint for direct API access.
//    Set to None when the provider has no standard single endpoint or
//    requires non-OpenAI auth (e.g. Anthropic, self-hosted Llama).
//  • Sources: official model cards / API documentation (March 2026).

static PROFILES: &[ModelProfile] = &[
    // ── OpenAI o-series ───────────────────────────────────────────────────
    ModelProfile {
        name: "o3-mini",
        context_tokens: 200_000,
        max_output_tokens: 100_000,
        base_url: Some(OPENAI_URL),
    },
    ModelProfile {
        name: "o3",
        context_tokens: 200_000,
        max_output_tokens: 100_000,
        base_url: Some(OPENAI_URL),
    },
    ModelProfile {
        name: "o1-mini",
        context_tokens: 128_000,
        max_output_tokens: 65_536,
        base_url: Some(OPENAI_URL),
    },
    ModelProfile {
        name: "o1-preview",
        context_tokens: 128_000,
        max_output_tokens: 32_768,
        base_url: Some(OPENAI_URL),
    },
    ModelProfile {
        name: "o1",
        context_tokens: 200_000,
        max_output_tokens: 100_000,
        base_url: Some(OPENAI_URL),
    },
    // ── OpenAI GPT-4o family ──────────────────────────────────────────────
    ModelProfile {
        name: "gpt-4o-mini",
        context_tokens: 128_000,
        max_output_tokens: 16_384,
        base_url: Some(OPENAI_URL),
    },
    ModelProfile {
        name: "gpt-4o",
        context_tokens: 128_000,
        max_output_tokens: 16_384,
        base_url: Some(OPENAI_URL),
    },
    // ── OpenAI GPT-4 family ───────────────────────────────────────────────
    ModelProfile {
        name: "gpt-4-turbo",
        context_tokens: 128_000,
        max_output_tokens: 4_096,
        base_url: Some(OPENAI_URL),
    },
    ModelProfile {
        name: "gpt-4-32k",
        context_tokens: 32_768,
        max_output_tokens: 4_096,
        base_url: Some(OPENAI_URL),
    },
    ModelProfile {
        name: "gpt-4",
        context_tokens: 8_192,
        max_output_tokens: 4_096,
        base_url: Some(OPENAI_URL),
    },
    // ── OpenAI GPT-3.5 ───────────────────────────────────────────────────
    ModelProfile {
        name: "gpt-3.5-turbo-16k",
        context_tokens: 16_384,
        max_output_tokens: 4_096,
        base_url: Some(OPENAI_URL),
    },
    ModelProfile {
        name: "gpt-3.5-turbo",
        context_tokens: 16_384,
        max_output_tokens: 4_096,
        base_url: Some(OPENAI_URL),
    },
    // ── Anthropic Claude ─────────────────────────────────────────────────
    // Anthropic's API is not natively OpenAI-compatible (different auth
    // headers); base_url is None — users must supply a compatible proxy URL
    // (e.g. AWS Bedrock, Vertex AI, or an openai-proxy sidecar) explicitly.
    ModelProfile {
        name: "claude-opus-4",
        context_tokens: 200_000,
        max_output_tokens: 32_000,
        base_url: None,
    },
    ModelProfile {
        name: "claude-sonnet-4",
        context_tokens: 200_000,
        max_output_tokens: 16_000,
        base_url: None,
    },
    ModelProfile {
        name: "claude-3-5-sonnet",
        context_tokens: 200_000,
        max_output_tokens: 8_192,
        base_url: None,
    },
    ModelProfile {
        name: "claude-3-5-haiku",
        context_tokens: 200_000,
        max_output_tokens: 8_192,
        base_url: None,
    },
    ModelProfile {
        name: "claude-3-opus",
        context_tokens: 200_000,
        max_output_tokens: 4_096,
        base_url: None,
    },
    ModelProfile {
        name: "claude-3-sonnet",
        context_tokens: 200_000,
        max_output_tokens: 4_096,
        base_url: None,
    },
    ModelProfile {
        name: "claude-3-haiku",
        context_tokens: 200_000,
        max_output_tokens: 4_096,
        base_url: None,
    },
    // ── DeepSeek ─────────────────────────────────────────────────────────
    ModelProfile {
        name: "deepseek-r1",
        context_tokens: 128_000,
        max_output_tokens: 64_000,
        base_url: Some(DEEPSEEK_URL),
    },
    ModelProfile {
        name: "deepseek-v3",
        context_tokens: 128_000,
        max_output_tokens: 8_192,
        base_url: Some(DEEPSEEK_URL),
    },
    ModelProfile {
        name: "deepseek-chat",
        context_tokens: 64_000,
        max_output_tokens: 8_192,
        base_url: Some(DEEPSEEK_URL),
    },
    // ── Alibaba Qwen (DashScope) ──────────────────────────────────────────
    ModelProfile {
        name: "qwen-long",
        context_tokens: 1_000_000,
        max_output_tokens: 8_192,
        base_url: Some(DASHSCOPE_URL),
    },
    ModelProfile {
        name: "qwen2.5-72b",
        context_tokens: 131_072,
        max_output_tokens: 8_192,
        base_url: Some(DASHSCOPE_URL),
    },
    ModelProfile {
        name: "qwen-plus",
        context_tokens: 131_072,
        max_output_tokens: 8_192,
        base_url: Some(DASHSCOPE_URL),
    },
    ModelProfile {
        name: "qwen-turbo",
        context_tokens: 131_072,
        max_output_tokens: 8_192,
        base_url: Some(DASHSCOPE_URL),
    },
    ModelProfile {
        name: "qwen-max",
        context_tokens: 32_768,
        max_output_tokens: 8_192,
        base_url: Some(DASHSCOPE_URL),
    },
    // ── Google Gemini ────────────────────────────────────────────────────
    ModelProfile {
        name: "gemini-2.0-flash",
        context_tokens: 1_048_576,
        max_output_tokens: 8_192,
        base_url: Some(GEMINI_URL),
    },
    ModelProfile {
        name: "gemini-1.5-pro",
        context_tokens: 2_097_152,
        max_output_tokens: 8_192,
        base_url: Some(GEMINI_URL),
    },
    ModelProfile {
        name: "gemini-1.5-flash",
        context_tokens: 1_048_576,
        max_output_tokens: 8_192,
        base_url: Some(GEMINI_URL),
    },
    ModelProfile {
        name: "gemini-1.0-pro",
        context_tokens: 32_768,
        max_output_tokens: 2_048,
        base_url: Some(GEMINI_URL),
    },
    // ── Meta Llama ───────────────────────────────────────────────────────
    // Open-weight models; no single canonical provider URL — users must
    // supply the endpoint (Together, Fireworks, Groq, self-hosted, …).
    ModelProfile {
        name: "llama-3.3-70b",
        context_tokens: 131_072,
        max_output_tokens: 4_096,
        base_url: None,
    },
    ModelProfile {
        name: "llama-3.1-405b",
        context_tokens: 131_072,
        max_output_tokens: 4_096,
        base_url: None,
    },
    ModelProfile {
        name: "llama-3.1-70b",
        context_tokens: 131_072,
        max_output_tokens: 4_096,
        base_url: None,
    },
    ModelProfile {
        name: "llama-3.1-8b",
        context_tokens: 131_072,
        max_output_tokens: 4_096,
        base_url: None,
    },
    ModelProfile {
        name: "llama-3-70b",
        context_tokens: 8_192,
        max_output_tokens: 4_096,
        base_url: None,
    },
    ModelProfile {
        name: "llama-3-8b",
        context_tokens: 8_192,
        max_output_tokens: 4_096,
        base_url: None,
    },
    // ── Mistral ──────────────────────────────────────────────────────────
    ModelProfile {
        name: "mistral-large",
        context_tokens: 131_072,
        max_output_tokens: 4_096,
        base_url: Some(MISTRAL_URL),
    },
    ModelProfile {
        name: "mistral-small",
        context_tokens: 131_072,
        max_output_tokens: 4_096,
        base_url: Some(MISTRAL_URL),
    },
    ModelProfile {
        name: "mixtral-8x22b",
        context_tokens: 65_536,
        max_output_tokens: 4_096,
        base_url: Some(MISTRAL_URL),
    },
    ModelProfile {
        name: "mixtral-8x7b",
        context_tokens: 32_768,
        max_output_tokens: 4_096,
        base_url: Some(MISTRAL_URL),
    },
    ModelProfile {
        name: "mistral-7b",
        context_tokens: 32_768,
        max_output_tokens: 4_096,
        base_url: Some(MISTRAL_URL),
    },
    // ── Moonshot Kimi ────────────────────────────────────────────────────
    ModelProfile {
        name: "kimi-k2.5",
        context_tokens: 131_072,
        max_output_tokens: 16_384,
        base_url: Some(MOONSHOT_URL),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_model_resolves() {
        let p = ModelProfile::for_model("gpt-4o-mini");
        assert_eq!(p.context_tokens, 128_000);
    }

    #[test]
    fn version_suffix_resolves() {
        // e.g. "gpt-4o-2024-08-06"
        let p = ModelProfile::for_model("gpt-4o-2024-08-06");
        assert_eq!(p.context_tokens, 128_000);
    }

    #[test]
    fn unknown_model_fallback() {
        let p = ModelProfile::for_model("some-custom-model-v99");
        assert_eq!(p.name, "<unknown>");
    }

    #[test]
    fn derived_values_in_range() {
        for p in PROFILES
            .iter()
            .chain(std::iter::once(&ModelProfile::FALLBACK))
        {
            let st = p.default_short_term_tokens();
            let ov = p.default_overflow_bytes();
            assert!(st >= 4_000 && st <= 64_000, "{}: short_term={st}", p.name);
            assert!(ov >= 8_192 && ov <= 200_000, "{}: overflow={ov}", p.name);
        }
    }

    #[test]
    fn base_url_lookup() {
        assert_eq!(
            ModelProfile::for_model("gpt-4o").base_url,
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            ModelProfile::for_model("deepseek-chat").base_url,
            Some("https://api.deepseek.com/v1")
        );
        assert_eq!(
            ModelProfile::for_model("qwen-turbo").base_url,
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1")
        );
        assert_eq!(
            ModelProfile::for_model("gemini-1.5-flash").base_url,
            Some("https://generativelanguage.googleapis.com/v1beta/openai")
        );
        assert_eq!(
            ModelProfile::for_model("mistral-large").base_url,
            Some("https://api.mistral.ai/v1")
        );
        assert_eq!(ModelProfile::for_model("claude-3-opus").base_url, None);
        assert_eq!(ModelProfile::for_model("llama-3.1-70b").base_url, None);
    }

    #[test]
    fn specificity_ordering() {
        // "gpt-4o-mini" must not accidentally match "gpt-4o" first
        let p = ModelProfile::for_model("gpt-4o-mini");
        assert_eq!(p.name, "gpt-4o-mini");
    }
}
