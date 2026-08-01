//! Official provider marks, embedded at build time.
//!
//! The first quick-pick shipped two-letter tiles instead of logos, citing
//! `design.md` §9's icon cut and trademark caution. The owner has since asked
//! for the real marks (2026-08-01): in a list of eighty-eight rows the vendor
//! mark is scanned rather than read, and the official shapes are the ones the
//! eye already knows from every other tool. The sources are the monochrome
//! single-path SVGs the simple-icons / lobehub sets distribute; they render
//! tinted into the text ramp, so they read as UI, not as advertising.
//!
//! Providers without a bundled mark — a custom OpenAI-compatible endpoint
//! (§10) — fall back to the caller's two-letter tile, which is why
//! [`provider_icon`] returns an `Option` rather than inventing a glyph.

use std::collections::HashMap;
use std::sync::OnceLock;

use iced::widget::svg;

/// One handle per file, created once: `Handle::from_memory` assigns a fresh id
/// per call, and a fresh id per frame would miss the renderer's cache and
/// re-tessellate the SVG at frame rate.
fn table() -> &'static HashMap<&'static str, svg::Handle> {
    static TABLE: OnceLock<HashMap<&'static str, svg::Handle>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map: HashMap<&'static str, svg::Handle> = HashMap::new();
        let mut put = |names: &[&'static str], bytes: &'static [u8]| {
            let handle = svg::Handle::from_memory(bytes);
            for name in names {
                map.insert(name, handle.clone());
            }
        };
        // Codex is the ChatGPT-subscription door to the same house.
        put(
            &["openai", "codex", "azure-openai"],
            include_bytes!("../assets/icons/openai.svg"),
        );
        put(
            &["anthropic"],
            include_bytes!("../assets/icons/anthropic.svg"),
        );
        put(&["gemini"], include_bytes!("../assets/icons/gemini.svg"));
        // Vertex is Google's serving path; the Gemini spark would claim the
        // model rather than the platform.
        put(
            &["vertex", "google"],
            include_bytes!("../assets/icons/google.svg"),
        );
        put(&["groq"], include_bytes!("../assets/icons/groq.svg"));
        put(
            &["cerebras"],
            include_bytes!("../assets/icons/cerebras.svg"),
        );
        put(&["xai"], include_bytes!("../assets/icons/xai.svg"));
        put(
            &["openrouter"],
            include_bytes!("../assets/icons/openrouter.svg"),
        );
        put(&["ollama"], include_bytes!("../assets/icons/ollama.svg"));
        put(
            &["deepseek"],
            include_bytes!("../assets/icons/deepseek.svg"),
        );
        put(&["mistral"], include_bytes!("../assets/icons/mistral.svg"));
        put(&["meta"], include_bytes!("../assets/icons/meta.svg"));
        put(&["qwen"], include_bytes!("../assets/icons/qwen.svg"));
        put(
            &["kimi", "moonshot"],
            include_bytes!("../assets/icons/kimi.svg"),
        );
        map
    })
}

/// The official mark for `provider`, if one is bundled.
pub fn provider_icon(provider: &str) -> Option<svg::Handle> {
    table().get(provider).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_providers_all_have_marks() {
        for provider in [
            "codex",
            "openai",
            "openrouter",
            "gemini",
            "anthropic",
            "groq",
            "cerebras",
            "xai",
            "vertex",
            "ollama",
        ] {
            assert!(provider_icon(provider).is_some(), "{provider}");
        }
    }

    #[test]
    fn an_unknown_provider_yields_no_icon_rather_than_a_wrong_one() {
        assert!(provider_icon("my-llama-box").is_none());
    }
}
