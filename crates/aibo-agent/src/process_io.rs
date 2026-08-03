//! Bounded line framing for subprocess protocols and diagnostics.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use std::ffi::{OsStr, OsString};

/// Largest JSON frame accepted from an agent subprocess.
pub(crate) const MAX_PROTOCOL_LINE_BYTES: usize = 1 << 20;
/// Largest single diagnostic line retained from child stderr.
pub(crate) const MAX_LOG_LINE_BYTES: usize = 64 << 10;

#[cfg(not(windows))]
const SAFE_CHILD_ENVIRONMENT: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM",
];

#[cfg(windows)]
const SAFE_CHILD_ENVIRONMENT: &[&str] = &[
    "PATH",
    "SystemRoot",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
];

/// The small ambient environment shared by managed agent subprocesses.
pub(crate) fn safe_child_environment() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| {
            SAFE_CHILD_ENVIRONMENT
                .iter()
                .any(|allowed| env_key_eq(key, OsStr::new(allowed)))
        })
        .collect()
}

#[cfg(windows)]
fn env_key_eq(left: &OsStr, right: &OsStr) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn env_key_eq(left: &OsStr, right: &OsStr) -> bool {
    left == right
}

/// One bounded line-read result.
pub(crate) enum BoundedLine {
    Line(String),
    TooLong,
    Eof,
}

/// Read through one newline without ever retaining more than `max` bytes.
pub(crate) async fn read_bounded_line<R>(reader: &mut R, max: usize) -> std::io::Result<BoundedLine>
where
    R: AsyncBufRead + Unpin,
{
    let mut retained = Vec::with_capacity(max.min(8 << 10));
    let mut too_long = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if retained.is_empty() && !too_long {
                return Ok(BoundedLine::Eof);
            }
            break;
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |at| at + 1);
        let content_len = newline.unwrap_or(available.len());
        if !too_long {
            let remaining = max.saturating_sub(retained.len());
            let take = remaining.min(content_len);
            retained.extend_from_slice(&available[..take]);
            too_long = take < content_len;
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }

    if too_long {
        return Ok(BoundedLine::TooLong);
    }
    if retained.last() == Some(&b'\r') {
        retained.pop();
    }
    Ok(BoundedLine::Line(
        String::from_utf8_lossy(&retained).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn oversized_lines_are_discarded_without_losing_the_next_frame() {
        let (client, mut server) = tokio::io::duplex(16);
        let writer = tokio::spawn(async move {
            server.write_all(b"0123456789\nok\n").await.unwrap();
        });
        let mut reader = BufReader::new(client);
        assert!(matches!(
            read_bounded_line(&mut reader, 4).await.unwrap(),
            BoundedLine::TooLong
        ));
        match read_bounded_line(&mut reader, 4).await.unwrap() {
            BoundedLine::Line(line) => assert_eq!(line, "ok"),
            _ => panic!("expected the next bounded line"),
        }
        writer.await.unwrap();
    }
}
