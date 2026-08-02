//! Agent working-directory candidates and recency (owner redesign,
//! 2026-08-02).
//!
//! The picker shows *recently used* directories first — "pick up where I
//! was" — then the configured roots and their immediate subdirectories. The
//! recents live in a plain state file (one absolute path per line, most
//! recent first), not in `config.toml`: they change on every run, and the
//! config file is the user's to edit, not a scratchpad.

use std::path::{Path, PathBuf};

/// How many recent directories are kept.
const RECENT_LIMIT: usize = 8;

/// How many immediate subdirectories one root may contribute. A `~/dev` with
/// three hundred checkouts should not turn the picker into a directory dump;
/// the filter field and the recents carry that weight.
const SUBDIRS_PER_ROOT: usize = 64;

/// Record `dir` as just used, most recent first, deduplicated and capped.
pub fn remember(state_file: &Path, dir: &Path) {
    let mut recents = recents(state_file);
    recents.retain(|known| known != dir);
    recents.insert(0, dir.to_path_buf());
    recents.truncate(RECENT_LIMIT);
    let body = recents
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if let Err(error) = std::fs::write(state_file, body) {
        tracing::warn!(%error, "could not persist recent workdirs");
    }
}

/// The recorded recents that still exist on disk, most recent first.
pub fn recents(state_file: &Path) -> Vec<PathBuf> {
    let Ok(body) = std::fs::read_to_string(state_file) else {
        return Vec::new();
    };
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .take(RECENT_LIMIT)
        .collect()
}

/// The pickable directories: each root, then its immediate subdirectories,
/// visible ones only, in the roots' configured order.
pub fn candidates(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        dirs.push(root.clone());
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !name.starts_with('.'))
            })
            .collect();
        subdirs.sort();
        subdirs.truncate(SUBDIRS_PER_ROOT);
        dirs.extend(subdirs);
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_dedupes_caps_and_survives_a_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("recent_workdirs");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();

        remember(&state, &a);
        remember(&state, &b);
        remember(&state, &a);
        assert_eq!(recents(&state), vec![a.clone(), b.clone()]);

        // A directory that vanished is not offered back.
        std::fs::remove_dir(&b).unwrap();
        assert_eq!(recents(&state), vec![a]);
    }

    #[test]
    fn candidates_list_each_root_then_its_visible_subdirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("beta")).unwrap();
        std::fs::create_dir(root.join("alpha")).unwrap();
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();

        let dirs = candidates(&[root.clone()]);
        assert_eq!(
            dirs,
            vec![root.clone(), root.join("alpha"), root.join("beta")],
            "root first, subdirs sorted, hidden and files excluded"
        );
    }
}
