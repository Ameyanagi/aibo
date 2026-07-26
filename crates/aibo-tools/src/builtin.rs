//! Tier 0: regex, JSON/base64/hash, case, diff (§11).
//!
//! Tier 0 is defined by what it *cannot* do: no I/O, no clock, no process, no
//! network, no allocation that depends on anything outside its arguments. That
//! is why §11 grants it "no consent, sub-ms" — the guarantee is structural, not
//! a policy that could be relaxed later. Every function here is pure.
//!
//! Date and unit arithmetic is tier 0 too, but lives in [`crate::compute`]
//! because it is `fend-core` rather than hand-written.
//!
//! Two deliberate choices:
//!
//! * **`regex`, not a backtracking engine.** A pattern arrives from a model or
//!   from untrusted captured text. `regex` runs in linear time, so a crafted
//!   pattern cannot turn a no-consent tool into a denial of service.
//! * **Input caps.** Every tool bounds its input length. Tier 0 shares the
//!   process with the UI thread's runtime; an unbounded 200 MB diff would blow
//!   the §15 latency budget even though it is "pure".

use std::sync::Arc;

use aibo_core::types::{ToolSchema, ToolTier};
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::json;
use sha2::Digest as _;
use tokio_util::sync::CancellationToken;

use crate::args::{failed, invalid, opt_bool, opt_str, str_arg};
use crate::{Tool, ToolOutput, ToolResult};

/// Largest input any tier-0 tool will accept, in bytes.
///
/// Tier 0 promises sub-ms (§1, §11). This cap is what makes that promise true
/// rather than aspirational.
pub const MAX_INPUT_BYTES: usize = 1 << 20; // 1 MiB

/// Largest compiled-program size a supplied regex may reach.
const MAX_REGEX_SIZE: usize = 1 << 20;

fn check_len(tool: &str, s: &str) -> ToolResult<()> {
    if s.len() > MAX_INPUT_BYTES {
        return Err(invalid(
            tool,
            format!(
                "input is {} bytes, limit {MAX_INPUT_BYTES} (tier 0 is a latency budget, not a batch job)",
                s.len()
            ),
        ));
    }
    Ok(())
}

/// Every tier-0 tool, ready to register.
pub fn all() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(RegexTool),
        Arc::new(JsonTool),
        Arc::new(Base64Tool),
        Arc::new(HashTool),
        Arc::new(CaseTool),
        Arc::new(DiffTool),
    ]
}

fn schema(name: &str, description: &str, parameters: serde_json::Value) -> ToolSchema {
    ToolSchema {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
        tier: 0,
    }
}

// ---------------------------------------------------------------------------
// regex
// ---------------------------------------------------------------------------

/// What [`regex_apply`] should do with the pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexAction {
    /// Does the pattern match anywhere?
    Test,
    /// Every non-overlapping match.
    FindAll,
    /// Named and numbered capture groups of the first match.
    Captures,
    /// Replace every match with a replacement template (`$1`, `$name`).
    ReplaceAll,
}

/// Case-insensitive / multi-line / dot-matches-newline flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegexFlags {
    /// `i`
    pub case_insensitive: bool,
    /// `m` — `^`/`$` match at line boundaries.
    pub multi_line: bool,
    /// `s` — `.` matches `\n`.
    pub dot_matches_new_line: bool,
}

impl RegexFlags {
    /// Parse a flag string such as `"ims"`. Unknown letters are an error so a
    /// silent typo cannot change matching semantics.
    pub fn parse(flags: &str) -> Result<Self, char> {
        let mut this = Self::default();
        for c in flags.chars() {
            match c {
                'i' => this.case_insensitive = true,
                'm' => this.multi_line = true,
                's' => this.dot_matches_new_line = true,
                other => return Err(other),
            }
        }
        Ok(this)
    }
}

/// Compile and apply a regular expression.
///
/// `replacement` is only read for [`RegexAction::ReplaceAll`].
pub fn regex_apply(
    pattern: &str,
    text: &str,
    action: RegexAction,
    flags: RegexFlags,
    replacement: &str,
) -> ToolResult<serde_json::Value> {
    check_len("regex", text)?;
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(flags.case_insensitive)
        .multi_line(flags.multi_line)
        .dot_matches_new_line(flags.dot_matches_new_line)
        .size_limit(MAX_REGEX_SIZE)
        .build()
        .map_err(|e| invalid("regex", e.to_string()))?;

    Ok(match action {
        RegexAction::Test => json!({ "matched": re.is_match(text) }),
        RegexAction::FindAll => {
            let found: Vec<serde_json::Value> = re
                .find_iter(text)
                .map(|m| json!({ "text": m.as_str(), "start": m.start(), "end": m.end() }))
                .collect();
            json!({ "count": found.len(), "matches": found })
        }
        RegexAction::Captures => match re.captures(text) {
            None => json!({ "matched": false }),
            Some(caps) => {
                let numbered: Vec<Option<&str>> =
                    caps.iter().map(|g| g.map(|m| m.as_str())).collect();
                let named: serde_json::Map<String, serde_json::Value> = re
                    .capture_names()
                    .flatten()
                    .map(|n| {
                        (
                            n.to_owned(),
                            caps.name(n)
                                .map(|m| json!(m.as_str()))
                                .unwrap_or(serde_json::Value::Null),
                        )
                    })
                    .collect();
                json!({ "matched": true, "groups": numbered, "named": named })
            }
        },
        RegexAction::ReplaceAll => {
            json!({ "text": re.replace_all(text, replacement).into_owned() })
        }
    })
}

/// Tier-0 regular expressions.
#[derive(Debug, Clone, Copy)]
pub struct RegexTool;

#[async_trait]
impl Tool for RegexTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "regex",
            "Test, search, capture or replace with a regular expression (linear-time engine; no backreferences or lookaround).",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "text": { "type": "string" },
                    "action": { "type": "string", "enum": ["test", "find_all", "captures", "replace_all"], "default": "find_all" },
                    "flags": { "type": "string", "description": "any of i, m, s" },
                    "replacement": { "type": "string", "description": "for replace_all; $1 and $name are expanded" }
                },
                "required": ["pattern", "text"]
            }),
        )
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Builtin
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let pattern = str_arg(&args, "regex", "pattern")?;
        let text = str_arg(&args, "regex", "text")?;
        let action = match opt_str(&args, "action").unwrap_or("find_all") {
            "test" => RegexAction::Test,
            "find_all" => RegexAction::FindAll,
            "captures" => RegexAction::Captures,
            "replace_all" => RegexAction::ReplaceAll,
            other => return Err(invalid("regex", format!("unknown action `{other}`"))),
        };
        let flags = RegexFlags::parse(opt_str(&args, "flags").unwrap_or_default())
            .map_err(|c| invalid("regex", format!("unknown flag `{c}`")))?;
        let replacement = opt_str(&args, "replacement").unwrap_or_default();
        let value = regex_apply(pattern, text, action, flags, replacement)?;
        Ok(ToolOutput::json(value.to_string(), value))
    }
}

// ---------------------------------------------------------------------------
// json
// ---------------------------------------------------------------------------

/// Validate, reformat or address into a JSON document.
///
/// `pointer` is RFC 6901 (`/a/0/b`), not a query language. A tier-0 tool must
/// not grow an expression evaluator: that would be a second, unbudgeted
/// interpreter with none of tier 1's limits.
pub fn json_apply(
    text: &str,
    action: &str,
    pointer: Option<&str>,
) -> ToolResult<serde_json::Value> {
    check_len("json", text)?;
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| invalid("json", e.to_string()))?;
    match action {
        "validate" => Ok(json!({ "valid": true })),
        "format" => Ok(json!({
            "text": serde_json::to_string_pretty(&doc).map_err(|e| failed("json", e.to_string()))?
        })),
        "minify" => Ok(json!({
            "text": serde_json::to_string(&doc).map_err(|e| failed("json", e.to_string()))?
        })),
        "pointer" => {
            let p = pointer.ok_or_else(|| invalid("json", "`pointer` is required"))?;
            match doc.pointer(p) {
                Some(v) => Ok(json!({ "found": true, "value": v })),
                None => Ok(json!({ "found": false })),
            }
        }
        other => Err(invalid("json", format!("unknown action `{other}`"))),
    }
}

/// Tier-0 JSON handling.
#[derive(Debug, Clone, Copy)]
pub struct JsonTool;

#[async_trait]
impl Tool for JsonTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "json",
            "Validate, pretty-print, minify or address into a JSON document with an RFC 6901 pointer.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "action": { "type": "string", "enum": ["validate", "format", "minify", "pointer"], "default": "format" },
                    "pointer": { "type": "string", "description": "RFC 6901 JSON pointer, e.g. /items/0/name" }
                },
                "required": ["text"]
            }),
        )
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Builtin
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let text = str_arg(&args, "json", "text")?;
        let action = opt_str(&args, "action").unwrap_or("format");
        let value = json_apply(text, action, opt_str(&args, "pointer"))?;
        let rendered = value
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        Ok(ToolOutput::json(rendered, value))
    }
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------

/// Base64-encode UTF-8 text. `url_safe` selects the RFC 4648 §5 alphabet.
pub fn base64_encode(text: &str, url_safe: bool) -> ToolResult<String> {
    check_len("base64", text)?;
    Ok(if url_safe {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text.as_bytes())
    } else {
        base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
    })
}

/// Base64-decode to UTF-8 text.
///
/// Non-UTF-8 payloads are an error rather than lossy text: a tool result feeds
/// straight into a prompt, and silently replacing bytes with `U+FFFD` would
/// make the model reason about data that was never there.
pub fn base64_decode(text: &str, url_safe: bool) -> ToolResult<String> {
    check_len("base64", text)?;
    let trimmed = text.trim();
    let bytes = if url_safe {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(trimmed)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(trimmed))
    } else {
        base64::engine::general_purpose::STANDARD.decode(trimmed)
    }
    .map_err(|e| invalid("base64", e.to_string()))?;
    String::from_utf8(bytes).map_err(|_| invalid("base64", "decoded bytes are not valid UTF-8"))
}

/// Tier-0 base64.
#[derive(Debug, Clone, Copy)]
pub struct Base64Tool;

#[async_trait]
impl Tool for Base64Tool {
    fn schema(&self) -> ToolSchema {
        schema(
            "base64",
            "Encode or decode base64 text, standard or URL-safe alphabet.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "action": { "type": "string", "enum": ["encode", "decode"], "default": "encode" },
                    "url_safe": { "type": "boolean", "default": false }
                },
                "required": ["text"]
            }),
        )
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Builtin
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let text = str_arg(&args, "base64", "text")?;
        let url_safe = opt_bool(&args, "url_safe").unwrap_or(false);
        let out = match opt_str(&args, "action").unwrap_or("encode") {
            "encode" => base64_encode(text, url_safe)?,
            "decode" => base64_decode(text, url_safe)?,
            other => return Err(invalid("base64", format!("unknown action `{other}`"))),
        };
        Ok(ToolOutput::json(out.clone(), json!({ "text": out })))
    }
}

// ---------------------------------------------------------------------------
// hash
// ---------------------------------------------------------------------------

/// Supported digests.
///
/// SHA-2 only. MD5 and SHA-1 are absent on purpose: a tool that hands a model
/// an MD5 invites it to present a broken digest as an integrity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    /// SHA-256.
    Sha256,
    /// SHA-384.
    Sha384,
    /// SHA-512.
    Sha512,
}

impl HashAlgorithm {
    /// Parse the wire name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sha256" => Some(Self::Sha256),
            "sha384" => Some(Self::Sha384),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }
}

/// Hex digest of UTF-8 text.
pub fn hash_text(text: &str, algorithm: HashAlgorithm) -> ToolResult<String> {
    check_len("hash", text)?;
    let bytes = text.as_bytes();
    Ok(match algorithm {
        HashAlgorithm::Sha256 => hex::encode(sha2::Sha256::digest(bytes)),
        HashAlgorithm::Sha384 => hex::encode(sha2::Sha384::digest(bytes)),
        HashAlgorithm::Sha512 => hex::encode(sha2::Sha512::digest(bytes)),
    })
}

/// Tier-0 hashing.
#[derive(Debug, Clone, Copy)]
pub struct HashTool;

#[async_trait]
impl Tool for HashTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "hash",
            "Hex SHA-2 digest of text (sha256, sha384, sha512).",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "algorithm": { "type": "string", "enum": ["sha256", "sha384", "sha512"], "default": "sha256" }
                },
                "required": ["text"]
            }),
        )
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Builtin
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let text = str_arg(&args, "hash", "text")?;
        let name = opt_str(&args, "algorithm").unwrap_or("sha256");
        let algorithm = HashAlgorithm::parse(name)
            .ok_or_else(|| invalid("hash", format!("unknown algorithm `{name}`")))?;
        let digest = hash_text(text, algorithm)?;
        Ok(ToolOutput::json(
            digest.clone(),
            json!({ "algorithm": name, "hex": digest }),
        ))
    }
}

// ---------------------------------------------------------------------------
// case
// ---------------------------------------------------------------------------

/// Identifier and prose casing styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseStyle {
    /// `lower case`
    Lower,
    /// `UPPER CASE`
    Upper,
    /// `Title Case`
    Title,
    /// `snake_case`
    Snake,
    /// `kebab-case`
    Kebab,
    /// `camelCase`
    Camel,
    /// `PascalCase`
    Pascal,
    /// `CONSTANT_CASE`
    Constant,
}

impl CaseStyle {
    /// Parse the wire name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "lower" => Some(Self::Lower),
            "upper" => Some(Self::Upper),
            "title" => Some(Self::Title),
            "snake" => Some(Self::Snake),
            "kebab" => Some(Self::Kebab),
            "camel" => Some(Self::Camel),
            "pascal" => Some(Self::Pascal),
            "constant" => Some(Self::Constant),
            _ => None,
        }
    }
}

/// Split an identifier or phrase into lowercase words.
///
/// Boundaries: any non-alphanumeric run, a lower→upper transition, and the
/// last capital of an acronym run followed by a lowercase letter
/// (`HTTPServer` → `http`, `server`).
fn words(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let starts_word = match prev {
            None => false,
            Some(p) => {
                (p.is_lowercase() && c.is_uppercase())
                    || (p.is_numeric() != c.is_numeric())
                    || (p.is_uppercase()
                        && c.is_uppercase()
                        && next.is_some_and(char::is_lowercase))
            }
        };
        if starts_word && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn capitalise(word: &str) -> String {
    let mut it = word.chars();
    match it.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + it.as_str(),
    }
}

/// Recase text.
pub fn convert_case(text: &str, style: CaseStyle) -> ToolResult<String> {
    check_len("case", text)?;
    // Lower/Upper are whole-string transforms: recasing prose must not eat its
    // punctuation, which word-splitting would.
    match style {
        CaseStyle::Lower => return Ok(text.to_lowercase()),
        CaseStyle::Upper => return Ok(text.to_uppercase()),
        _ => {}
    }
    let w = words(text);
    Ok(match style {
        CaseStyle::Lower | CaseStyle::Upper => unreachable!("handled above"),
        CaseStyle::Title => w
            .iter()
            .map(|s| capitalise(s))
            .collect::<Vec<_>>()
            .join(" "),
        CaseStyle::Snake => w.join("_"),
        CaseStyle::Kebab => w.join("-"),
        CaseStyle::Constant => w.join("_").to_uppercase(),
        CaseStyle::Pascal => w.iter().map(|s| capitalise(s)).collect(),
        CaseStyle::Camel => w
            .iter()
            .enumerate()
            .map(|(i, s)| if i == 0 { s.clone() } else { capitalise(s) })
            .collect(),
    })
}

/// Tier-0 case conversion.
#[derive(Debug, Clone, Copy)]
pub struct CaseTool;

#[async_trait]
impl Tool for CaseTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "case",
            "Convert text between lower, upper, title, snake, kebab, camel, pascal and constant case.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "style": { "type": "string", "enum": ["lower", "upper", "title", "snake", "kebab", "camel", "pascal", "constant"] }
                },
                "required": ["text", "style"]
            }),
        )
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Builtin
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let text = str_arg(&args, "case", "text")?;
        let name = str_arg(&args, "case", "style")?;
        let style = CaseStyle::parse(name)
            .ok_or_else(|| invalid("case", format!("unknown style `{name}`")))?;
        let out = convert_case(text, style)?;
        Ok(ToolOutput::json(out.clone(), json!({ "text": out })))
    }
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

/// A unified diff between two texts.
///
/// This is the *display* diff — the one shown in the §11 "exact command and
/// diff preview before writes" panel. It is not a patch format aibo applies;
/// nothing in this crate consumes its output.
pub fn unified_diff(
    old: &str,
    new: &str,
    old_label: &str,
    new_label: &str,
    context: usize,
) -> ToolResult<String> {
    check_len("diff", old)?;
    check_len("diff", new)?;
    let diff = similar::TextDiff::from_lines(old, new);
    Ok(diff
        .unified_diff()
        .context_radius(context)
        .header(old_label, new_label)
        .to_string())
}

/// Tier-0 diffing.
#[derive(Debug, Clone, Copy)]
pub struct DiffTool;

#[async_trait]
impl Tool for DiffTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "diff",
            "Unified line diff between two texts.",
            json!({
                "type": "object",
                "properties": {
                    "old": { "type": "string" },
                    "new": { "type": "string" },
                    "old_label": { "type": "string", "default": "old" },
                    "new_label": { "type": "string", "default": "new" },
                    "context": { "type": "integer", "minimum": 0, "default": 3 }
                },
                "required": ["old", "new"]
            }),
        )
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Builtin
    }

    async fn call(&self, args: serde_json::Value, _c: CancellationToken) -> ToolResult<ToolOutput> {
        let old = str_arg(&args, "diff", "old")?;
        let new = str_arg(&args, "diff", "new")?;
        let context = args
            .get("context")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(3)
            .min(64) as usize;
        let text = unified_diff(
            old,
            new,
            opt_str(&args, "old_label").unwrap_or("old"),
            opt_str(&args, "new_label").unwrap_or("new"),
            context,
        )?;
        let identical = text.is_empty();
        Ok(ToolOutput::json(
            text.clone(),
            json!({ "identical": identical, "diff": text }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_find_all_reports_spans() {
        let v = regex_apply(
            r"\d+",
            "a1 bb22",
            RegexAction::FindAll,
            RegexFlags::default(),
            "",
        )
        .unwrap();
        assert_eq!(v["count"], 2);
        assert_eq!(v["matches"][1]["text"], "22");
        assert_eq!(v["matches"][1]["start"], 5);
    }

    #[test]
    fn regex_captures_named_groups() {
        let v = regex_apply(
            r"(?<year>\d{4})-(?<month>\d{2})",
            "on 2026-07-26",
            RegexAction::Captures,
            RegexFlags::default(),
            "",
        )
        .unwrap();
        assert_eq!(v["matched"], true);
        assert_eq!(v["named"]["year"], "2026");
        assert_eq!(v["named"]["month"], "07");
    }

    #[test]
    fn regex_flags_are_validated_not_ignored() {
        assert_eq!(RegexFlags::parse("q"), Err('q'));
        assert!(RegexFlags::parse("ims").unwrap().dot_matches_new_line);
    }

    #[test]
    fn a_bad_pattern_is_an_error_not_a_panic() {
        let err = regex_apply("(", "x", RegexAction::Test, RegexFlags::default(), "").unwrap_err();
        assert!(matches!(err, crate::ToolError::InvalidArguments { .. }));
    }

    #[test]
    fn oversized_input_is_refused() {
        let big = "x".repeat(MAX_INPUT_BYTES + 1);
        assert!(hash_text(&big, HashAlgorithm::Sha256).is_err());
    }

    #[test]
    fn json_pointer_addresses_into_the_document() {
        let v = json_apply(
            r#"{"items":[{"name":"a"}]}"#,
            "pointer",
            Some("/items/0/name"),
        )
        .unwrap();
        assert_eq!(v["found"], true);
        assert_eq!(v["value"], "a");

        let miss = json_apply(r#"{"a":1}"#, "pointer", Some("/nope")).unwrap();
        assert_eq!(miss["found"], false);
    }

    #[test]
    fn base64_round_trips_including_url_safe() {
        for url_safe in [false, true] {
            let enc = base64_encode("héllo?/+", url_safe).unwrap();
            assert_eq!(base64_decode(&enc, url_safe).unwrap(), "héllo?/+");
        }
    }

    #[test]
    fn base64_refuses_non_utf8_payloads() {
        let enc = base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe]);
        assert!(base64_decode(&enc, false).is_err());
    }

    #[test]
    fn sha256_matches_the_known_empty_digest() {
        assert_eq!(
            hash_text("", HashAlgorithm::Sha256).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn case_conversion_handles_acronyms_and_digits() {
        assert_eq!(
            convert_case("HTTPServer v2", CaseStyle::Snake).unwrap(),
            "http_server_v_2"
        );
        assert_eq!(
            convert_case("hello world", CaseStyle::Camel).unwrap(),
            "helloWorld"
        );
        assert_eq!(
            convert_case("hello world", CaseStyle::Pascal).unwrap(),
            "HelloWorld"
        );
        assert_eq!(
            convert_case("some-mixed_input", CaseStyle::Constant).unwrap(),
            "SOME_MIXED_INPUT"
        );
        assert_eq!(
            convert_case("some-mixed_input", CaseStyle::Kebab).unwrap(),
            "some-mixed-input"
        );
    }

    #[test]
    fn lower_and_upper_keep_punctuation() {
        assert_eq!(
            convert_case("Hello, World!", CaseStyle::Lower).unwrap(),
            "hello, world!"
        );
        assert_eq!(
            convert_case("Hello, World!", CaseStyle::Upper).unwrap(),
            "HELLO, WORLD!"
        );
    }

    #[test]
    fn identical_texts_diff_to_nothing() {
        assert!(
            unified_diff("a\nb\n", "a\nb\n", "old", "new", 3)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_changed_line_shows_up_in_the_diff() {
        let d = unified_diff("a\nb\n", "a\nc\n", "old", "new", 1).unwrap();
        assert!(d.contains("-b"), "{d}");
        assert!(d.contains("+c"), "{d}");
    }

    #[tokio::test]
    async fn every_builtin_answers_through_the_trait() {
        let reg = crate::ToolRegistry::with_builtins();
        let cancel = CancellationToken::new();
        let cases = [
            (
                "regex",
                json!({"pattern": "a", "text": "abc", "action": "test"}),
            ),
            ("json", json!({"text": "{\"a\":1}", "action": "minify"})),
            ("base64", json!({"text": "hi"})),
            ("hash", json!({"text": "hi"})),
            ("case", json!({"text": "hi there", "style": "snake"})),
            ("diff", json!({"old": "a\n", "new": "b\n"})),
        ];
        for (name, args) in cases {
            let out = reg.call(name, args, cancel.clone()).await.unwrap();
            assert!(!out.is_error, "{name} reported an error: {}", out.text);
            assert_eq!(out.origin(), aibo_core::types::ContentOrigin::ToolResult);
        }
        assert_eq!(reg.len(), cases_len());
    }

    fn cases_len() -> usize {
        all().len()
    }
}
