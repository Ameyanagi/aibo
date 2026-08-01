//! The `@` file finder's runtime half (§P9+): a bounded walk of the
//! configured roots, and a size-capped text read of one picked file.
//!
//! The walk and the read live here — not in the UI — because file access is
//! authority, and §5 keeps authority runtime-side. What crosses the bridge is
//! a list of paths one way and one bounded, decoded string the other; the
//! string then rides the fenced selection pipeline like any other untrusted
//! capture.

use std::path::{Path, PathBuf};

use aibo_ui::UiEvent;
use aibo_ui::bridge::FileCandidate;

/// Directory depth below each root the walk descends.
const MAX_DEPTH: usize = 5;

/// Total candidates across all roots. Enough for a working home directory;
/// a bound because an unbounded walk of a mounted archive is a hang.
const MAX_CANDIDATES: usize = 20_000;

/// The largest file the attach path will read. Bigger than any §5 context
/// budget can use, small enough that a stray binary costs nothing.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// Directory names never descended into: dependency and build trees would
/// swamp the candidate budget with files nobody attaches.
const SKIPPED_DIRS: &[&str] = &["node_modules", "target", "Library", ".git", ".venv", "venv"];

/// The home directory across both shipping targets: `HOME` on Unix,
/// `USERPROFILE` on Windows. Missing this made the finder walk *nothing* on
/// Windows, which CI caught before a user could.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The roots to index: the configured `[files] roots`, else Documents,
/// Desktop and Downloads under the home directory. `~/` prefixes expand.
pub fn roots(configured: Option<&[String]>) -> Vec<PathBuf> {
    roots_with_home(configured, home_dir())
}

fn roots_with_home(configured: Option<&[String]>, home: Option<PathBuf>) -> Vec<PathBuf> {
    match configured {
        Some(roots) => roots
            .iter()
            .filter_map(|root| match (root.strip_prefix("~/"), &home) {
                (Some(rest), Some(home)) => Some(home.join(rest)),
                (Some(_), None) => None,
                (None, _) => Some(PathBuf::from(root)),
            })
            .collect(),
        None => home
            .map(|home| {
                ["Documents", "Desktop", "Downloads"]
                    .into_iter()
                    .map(|dir| home.join(dir))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Walk `roots` breadth-first into a bounded candidate list.
///
/// Blocking by design — call it from `spawn_blocking`. Hidden entries are
/// skipped: dotfiles are configuration, not documents, and the depth cap
/// already keeps the walk out of most machinery.
pub fn walk(roots: &[PathBuf]) -> Vec<FileCandidate> {
    let home = home_dir();
    let mut out = Vec::new();
    // A real queue, popped from the front. The first cut of this used
    // `Vec::pop`, which is depth-first from the LAST root — Downloads alone
    // could eat the whole candidate budget before Documents was ever visited,
    // and a file that is not in the index cannot be found no matter how good
    // the matcher is (owner report: `tesutofo` missing テスト　フォローアップ).
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        roots.iter().map(|root| (root.clone(), 0)).collect();

    while let Some((dir, depth)) = queue.pop_front() {
        if out.len() >= MAX_CANDIDATES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_CANDIDATES {
                break;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if depth < MAX_DEPTH && !SKIPPED_DIRS.contains(&name) {
                    queue.push_back((path, depth + 1));
                }
            } else if file_type.is_file() {
                out.push(FileCandidate {
                    display: display_path(&path, home.as_deref()),
                    path: path.to_string_lossy().into_owned(),
                });
            }
        }
    }
    out
}

/// Read one picked file as bounded text.
///
/// `Err` carries no detail on purpose: the UI's toast names the file, the
/// reason lives in diagnostics, and §13 never renders a raw error.
pub fn read_bounded(path: &Path) -> Result<String, ()> {
    let metadata = std::fs::metadata(path).map_err(|_| ())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(());
    }
    let bytes = std::fs::read(path).map_err(|_| ())?;
    if bytes.contains(&0) {
        // A NUL byte is the honest binary test for the formats people attach.
        return Err(());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The event for one attach attempt, ready to emit.
pub fn attach_event(path: &Path) -> UiEvent {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    match read_bounded(path) {
        Ok(content) => UiEvent::FileAttached { name, content },
        Err(()) => UiEvent::FileAttachFailed { name },
    }
}

fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = path.strip_prefix(home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_walk_is_bounded_and_skips_hidden_and_machinery() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("視覚資料.txt"), "x").expect("write");
        std::fs::write(root.path().join(".hidden"), "x").expect("write");
        std::fs::create_dir(root.path().join("node_modules")).expect("mkdir");
        std::fs::write(root.path().join("node_modules").join("dep.js"), "x").expect("write");
        std::fs::create_dir(root.path().join("docs")).expect("mkdir");
        std::fs::write(root.path().join("docs").join("note.md"), "x").expect("write");

        let files = walk(&[root.path().to_path_buf()]);
        let names: Vec<&str> = files.iter().map(|f| f.display.as_str()).collect();
        assert_eq!(files.len(), 2, "{names:?}");
        assert!(names.iter().any(|n| n.contains("視覚資料.txt")));
        assert!(names.iter().any(|n| n.contains("note.md")));
    }

    /// The walk order that keeps one huge root from starving the others: all
    /// shallow entries across every root land before any deep tree.
    #[test]
    fn every_root_is_visited_before_any_deep_tree() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        let deep = b.path().join("d1").join("d2");
        std::fs::create_dir_all(&deep).expect("mkdirs");
        std::fs::write(deep.join("deep.txt"), "x").expect("write");
        std::fs::write(a.path().join("shallow.txt"), "x").expect("write");

        let files = walk(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let shallow = files
            .iter()
            .position(|f| f.display.ends_with("shallow.txt"))
            .expect("shallow indexed");
        let deep = files
            .iter()
            .position(|f| f.display.ends_with("deep.txt"))
            .expect("deep indexed");
        assert!(
            shallow < deep,
            "breadth-first: the second root's shallow file must precede the deep tree"
        );
    }

    #[test]
    fn binary_and_oversized_files_are_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let binary = root.path().join("a.bin");
        std::fs::write(&binary, [0u8, 159, 146, 150]).expect("write");
        assert!(read_bounded(&binary).is_err());

        let text = root.path().join("a.txt");
        std::fs::write(&text, "こんにちは").expect("write");
        assert_eq!(read_bounded(&text).expect("text"), "こんにちは");
    }

    /// Deterministic across platforms: the previous version read the real
    /// `HOME`, which does not exist on Windows and failed CI there.
    #[test]
    fn tilde_roots_expand_and_defaults_exist() {
        let home = Some(PathBuf::from("/home/x"));
        let roots = roots_with_home(
            Some(&["~/Documents".to_owned(), "/tmp".to_owned()]),
            home.clone(),
        );
        assert_eq!(
            roots,
            vec![PathBuf::from("/home/x/Documents"), PathBuf::from("/tmp")]
        );
        assert_eq!(roots_with_home(None, home).len(), 3, "the default roots");
        assert!(roots_with_home(None, None).is_empty(), "no home, no walk");
    }
}
