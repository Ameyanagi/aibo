//! The payloads. Each one exists because the plan names a specific failure.
//!
//! §20 S4 asks for *"Paste-and-restore vs `SendInput`/`CGEventPost` across the
//! app set, Unicode and 5 KB inserts. Does clipboard save/restore round-trip?"*
//! §8 is more specific about how the synthetic path breaks on macOS, and each of
//! those claims is a payload here rather than a sentence in a README.

/// One insert test case.
#[derive(Debug, Clone)]
pub struct Payload {
    /// Short id used on the command line and in the report.
    pub id: &'static str,
    /// The text to insert.
    pub text: String,
    /// Why this payload exists — printed with the result so a failure is
    /// self-explaining.
    pub why: &'static str,
}

/// Every payload, in the order worth running them.
pub fn all() -> Vec<Payload> {
    vec![
        Payload {
            id: "ascii",
            text: "The quick brown fox jumps over the lazy dog.".to_owned(),
            why: "baseline. If this fails, nothing else below means anything.",
        },
        Payload {
            id: "japanese",
            text: "明日の会議は三時からです。よろしくお願いいたします。".to_owned(),
            why: "BMP non-ASCII. Also the payload to re-run while an IME is active (see S7).",
        },
        Payload {
            id: "emoji",
            text: "family 👨‍👩‍👧‍👦 flag 🇯🇵 astral 𠀋 done".to_owned(),
            why: "§8: enigo has open Unicode bugs including \"emoji typing the wrong character on macOS\". \
                  Surrogate pairs and ZWJ sequences are where CGEvent::set_string chunking corrupts text.",
        },
        Payload {
            id: "combining",
            text: "cafe\u{0301} nai\u{0308}ve re\u{0301}sume\u{0301}".to_owned(),
            why: "combining marks. A chunk boundary that lands between a base char and its \
                  combining mark produces visibly wrong text.",
        },
        Payload {
            id: "leading-newline",
            text: "\nsecond line begins after a leading newline".to_owned(),
            why: "§8: enigo's set_string \"silently fails on chunks starting with a newline\" \
                  (it carries a U+200B workaround). This is the smallest reproduction.",
        },
        Payload {
            id: "newline-at-chunk-boundary",
            text: newline_at_chunk_boundary(),
            why: "§8: the chunk size is 20 characters. This payload puts a '\\n' at exactly \
                  character 20, 60 and 100 so a chunk STARTS with a newline. The one-liner above \
                  can be caught by a naive guard; this one cannot.",
        },
        Payload {
            id: "multiline",
            text: "line one\nline two\n\nline four after a blank\n\ttab indented".to_owned(),
            why: "newlines and a tab in a normal-looking body. In a chat app a newline may SEND \
                  the message instead of inserting — that is a product-level finding, not a bug.",
        },
        Payload {
            id: "whitespace-edges",
            text: "  leading and trailing spaces preserved  ".to_owned(),
            why: "§5: \"a stripped space is a visible bug\". Some paths trim.",
        },
        Payload {
            id: "5kb",
            text: filler(5 * 1024),
            why: "§20 asks for a 5 KB insert explicitly. Watch for truncation, for the app \
                  freezing, and for how long it takes — a 5 KB synthetic type is thousands of events.",
        },
        Payload {
            id: "5kb-unicode",
            text: filler_ja(5 * 1024),
            why: "5 KB where bytes, UTF-16 units and graphemes all differ. Truncation bugs that \
                  survive ASCII show up here.",
        },
    ]
}

/// Look up a payload by id.
pub fn find(id: &str) -> Option<Payload> {
    all().into_iter().find(|p| p.id == id)
}

/// A string with `\n` at every 20-character boundary.
///
/// enigo chunks `CGEvent::set_string` at 20 characters, and §8 says a chunk that
/// *starts* with a newline is silently dropped. Placing the newline at index 20,
/// 60, 100 … makes it the first character of chunks 2, 4, 6 …
fn newline_at_chunk_boundary() -> String {
    const CHUNK: usize = 20;
    let mut out = String::new();
    for block in 0..5 {
        // Exactly 20 characters of filler, so the newline lands on the boundary.
        out.push_str(&format!("block{block:02}-abcdefghijkl"));
        debug_assert_eq!(out.chars().count() % CHUNK, 0, "filler must be 20 chars");
        out.push('\n');
        // The '\n' just consumed position 0 of the next chunk; pad the rest.
        out.push_str("rest of the chunk!!");
    }
    out
}

/// ASCII filler of approximately `bytes` bytes, with line structure so a partial
/// insert is visibly identifiable rather than just "shorter".
fn filler(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes + 64);
    let mut line = 0usize;
    while out.len() < bytes {
        out.push_str(&format!(
            "{line:04} the quick brown fox jumps over the lazy dog and keeps going.\n"
        ));
        line += 1;
    }
    out
}

/// Japanese filler of approximately `bytes` bytes.
fn filler_ja(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes + 64);
    let mut line = 0usize;
    while out.len() < bytes {
        out.push_str(&format!(
            "{line:04}行目：この文章はUnicodeの取り扱いを確認するための埋め草です。絵文字🙂も含みます。\n"
        ));
        line += 1;
    }
    out
}

/// The three counts that matter, so a partial insert can be diagnosed.
///
/// A result that is short by a fixed number of *UTF-16 units* points at range
/// arithmetic; short by *graphemes* points at chunking; short by a round number
/// of characters points at a buffer cap in the target app.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Counts {
    /// UTF-8 bytes.
    pub bytes: usize,
    /// UTF-16 code units — what `kAXSelectedTextRangeAttribute` speaks.
    pub utf16: usize,
    /// Unicode scalar values.
    pub chars: usize,
}

/// Measure a string.
pub fn counts(text: &str) -> Counts {
    Counts {
        bytes: text.len(),
        utf16: text.encode_utf16().count(),
        chars: text.chars().count(),
    }
}

/// Where two strings first diverge, as a character index, plus context.
///
/// Reporting "expected 5120 chars, got 5100" is nearly useless; reporting
/// "diverged at char 1240, expected '\n' got 'r'" points straight at the chunk
/// boundary that dropped.
pub fn first_divergence(expected: &str, actual: &str) -> Option<(usize, String)> {
    let mut e = expected.chars();
    let mut a = actual.chars();
    let mut index = 0usize;
    loop {
        match (e.next(), a.next()) {
            (None, None) => return None,
            (Some(x), Some(y)) if x == y => index += 1,
            (x, y) => {
                let window_expected: String = expected
                    .chars()
                    .skip(index.saturating_sub(8))
                    .take(24)
                    .collect();
                let window_actual: String = actual
                    .chars()
                    .skip(index.saturating_sub(8))
                    .take(24)
                    .collect();
                return Some((
                    index,
                    format!(
                        "expected {:?} got {:?}\n      …{:?}…\n      …{:?}…",
                        x, y, window_expected, window_actual
                    ),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_payload_has_a_unique_id() {
        let mut ids: Vec<&str> = all().iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn the_chunk_boundary_payload_actually_puts_newlines_on_the_boundary() {
        let text = newline_at_chunk_boundary();
        let chars: Vec<char> = text.chars().collect();
        // Chunk size 20: indices 20, 60, 100 … must be '\n' for the payload to
        // be testing what it claims. If enigo changes its chunk size this test
        // is the thing that tells you the payload went stale.
        assert_eq!(chars[20], '\n', "chunk 2 must start with a newline");
        assert!(chars.len() > 60);
        assert_eq!(chars[60], '\n', "chunk 4 must start with a newline");
    }

    #[test]
    fn the_five_kb_payloads_are_at_least_five_kb() {
        assert!(find("5kb").unwrap().text.len() >= 5 * 1024);
        assert!(find("5kb-unicode").unwrap().text.len() >= 5 * 1024);
    }

    #[test]
    fn unicode_payload_counts_all_differ() {
        let c = counts(&find("emoji").unwrap().text);
        assert!(c.bytes > c.utf16 && c.utf16 > c.chars, "{c:?}");
    }

    #[test]
    fn divergence_points_at_the_first_wrong_character() {
        assert_eq!(first_divergence("hello", "hello"), None);
        let (index, _) = first_divergence("hello world", "hello worlt").unwrap();
        assert_eq!(index, 10);
        let (index, _) = first_divergence("abc", "ab").unwrap();
        assert_eq!(index, 2, "a truncated insert diverges at the cut");
    }
}
