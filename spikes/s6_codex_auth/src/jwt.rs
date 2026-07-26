//! Minimal, *non-verifying* JWT payload reader.
//!
//! S6 only needs one claim out of the ID token — `chatgpt_account_id`, which
//! §3a says feeds the mandatory `ChatGPT-Account-ID` header. The spike does not
//! verify the signature and must not be copied into product code that does
//! anything security-relevant with the result.

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Decode the payload segment of a JWT into JSON, without verifying anything.
pub fn decode_payload(token: &str) -> Result<serde_json::Value> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("token has no payload segment"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .context("payload segment is not base64url")?;
    serde_json::from_slice(&bytes).context("payload segment is not JSON")
}

/// Pull `chatgpt_account_id` out of an ID token payload.
///
/// Codex reads it from a namespaced auth claim rather than the top level, so
/// both shapes are searched. SPIKE: S6 — record which shape the live token
/// actually uses; the answer decides what `aibo-provider`'s `codex` module
/// parses.
pub fn chatgpt_account_id(payload: &serde_json::Value) -> Option<String> {
    const NAMESPACES: &[&str] = &[
        "https://api.openai.com/auth",
        "https://api.openai.com/profile",
    ];

    if let Some(id) = payload.get("chatgpt_account_id").and_then(|v| v.as_str()) {
        return Some(id.to_owned());
    }
    for ns in NAMESPACES {
        if let Some(id) = payload
            .get(*ns)
            .and_then(|v| v.get("chatgpt_account_id"))
            .and_then(|v| v.as_str())
        {
            return Some(id.to_owned());
        }
    }
    // Last resort: a recursive scan, so an unexpected nesting still yields the
    // claim rather than failing the whole spike.
    scan(payload, "chatgpt_account_id")
}

/// The ChatGPT plan type, when the token carries one. Useful context for the
/// go/no-go writeup: a rejection on a Free account proves much less than a
/// rejection on a Pro one.
pub fn plan_type(payload: &serde_json::Value) -> Option<String> {
    scan(payload, "chatgpt_plan_type")
}

fn scan(value: &serde_json::Value, key: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(|v| v.as_str()) {
                return Some(found.to_owned());
            }
            map.values().find_map(|v| scan(v, key))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|v| scan(v, key)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_namespaced_claim() {
        let payload = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_123" }
        });
        assert_eq!(chatgpt_account_id(&payload).as_deref(), Some("acct_123"));
    }

    #[test]
    fn finds_a_top_level_claim() {
        let payload = serde_json::json!({ "chatgpt_account_id": "acct_456" });
        assert_eq!(chatgpt_account_id(&payload).as_deref(), Some("acct_456"));
    }

    #[test]
    fn finds_a_deeply_nested_claim() {
        let payload = serde_json::json!({ "a": { "b": { "chatgpt_account_id": "acct_789" } } });
        assert_eq!(chatgpt_account_id(&payload).as_deref(), Some("acct_789"));
    }
}
