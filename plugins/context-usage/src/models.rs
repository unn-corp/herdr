//! Best-effort model to context-window-size registry.
//!
//! Some agents report token counts but not the model's context window (Codex
//! reports the window directly and does not need this; OpenCode does). This maps
//! a model id to a context-window size so a percentage can be shown. It is
//! deliberately conservative: an unknown model returns `None` and the collector
//! reports token counts with `estimated`/no-percentage rather than guessing.

/// Context window (in tokens) for a model id, by longest-prefix/substring match.
/// Returns `None` when the model is unknown.
pub fn context_window(model_id: &str) -> Option<u64> {
    let m = model_id.to_ascii_lowercase();
    // Ordered from most to least specific. First substring hit wins.
    const TABLE: &[(&str, u64)] = &[
        // Anthropic
        ("claude-3-5-haiku", 200_000),
        ("claude-haiku-4", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-opus-4", 200_000),
        ("claude-3", 200_000),
        ("claude", 200_000),
        // OpenAI
        ("gpt-4.1", 1_047_576),
        ("gpt-4o", 128_000),
        ("gpt-5", 400_000),
        ("o4", 200_000),
        ("o3", 200_000),
        ("o1", 200_000),
        // Google
        ("gemini-2.5", 1_048_576),
        ("gemini-1.5", 1_048_576),
        ("gemini", 1_048_576),
        // xAI Grok Build (models_cache is preferred when present; these are fallbacks)
        ("grok-4", 500_000),
        ("grok-composer", 200_000),
        ("grok", 500_000),
        // Meta / Mistral / Qwen / DeepSeek / Nemotron (common open weights)
        ("llama-3", 128_000),
        ("qwen", 131_072),
        ("deepseek", 131_072),
        ("mistral", 131_072),
        ("nemotron", 131_072),
        ("kimi", 131_072),
        ("glm", 131_072),
    ];
    TABLE
        .iter()
        .find(|(needle, _)| m.contains(needle))
        .map(|(_, window)| *window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_resolve() {
        assert_eq!(context_window("claude-sonnet-4-20250514"), Some(200_000));
        assert_eq!(context_window("gpt-5.5"), Some(400_000));
        assert_eq!(context_window("gemini-2.5-pro"), Some(1_048_576));
        assert_eq!(context_window("nemotron-3-ultra-free"), Some(131_072));
        assert_eq!(context_window("grok-4.5"), Some(500_000));
        assert_eq!(context_window("grok-composer-2.5-fast"), Some(200_000));
    }

    #[test]
    fn unknown_model_is_none() {
        assert_eq!(context_window("some-bespoke-model-xyz"), None);
    }
}
