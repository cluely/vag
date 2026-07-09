//! Background directory walk feeding the new-session directory picker.
//!
//! BFS from a root (normally `$HOME`), depth- and count-capped so the walk
//! finishes in well under a second on a typical home directory. Results are
//! emitted in batches as tilde-abbreviated strings — shallow directories
//! first, which doubles as a sane default ordering before any query is
//! typed.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;

/// Hard cap on emitted directories — a runaway tree stops here.
pub const MAX_DIRS: usize = 30_000;
/// Levels below the root that are walked.
const MAX_DEPTH: usize = 5;
/// Directories per emitted batch.
const BATCH: usize = 512;
/// Never descended into: dependency/output trees with huge fan-out and no
/// plausible session cwd inside. Hidden directories are skipped wholesale.
const SKIP_NAMES: &[&str] = &["node_modules", "__pycache__"];

/// Walk `root` and hand batches of candidate directories to `emit`.
/// `skip` holds already-seeded candidates (session cwds) so they aren't
/// emitted twice; they are still descended into.
pub fn scan(root: &Path, home: &Path, skip: &HashSet<String>, emit: &mut dyn FnMut(Vec<String>)) {
    let mut queue: VecDeque<(std::path::PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    let mut batch = Vec::with_capacity(BATCH);
    let mut count = 0usize;

    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // Symlinks are never followed: they alias trees found elsewhere
            // and can form cycles.
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') || SKIP_NAMES.contains(&name) {
                continue;
            }
            let path = entry.path();
            // macOS ~/Library: enormous and never a working directory.
            if depth == 0 && root == home && name == "Library" {
                continue;
            }
            // Build/cache output trees mark themselves (cargo target/, etc.).
            if path.join("CACHEDIR.TAG").exists() {
                continue;
            }
            let display = abbrev_home(&path, home);
            if !skip.contains(&display) {
                batch.push(display);
                count += 1;
                if batch.len() >= BATCH {
                    emit(std::mem::take(&mut batch));
                }
                if count >= MAX_DIRS {
                    emit(batch);
                    return;
                }
            }
            if depth + 1 < MAX_DEPTH {
                queue.push_back((path, depth + 1));
            }
        }
    }
    if !batch.is_empty() {
        emit(batch);
    }
}

/// `/Users/me/x` → `~/x`; paths outside `home` stay absolute.
pub fn abbrev_home(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(root: &Path, home: &Path) -> Vec<String> {
        let mut out = Vec::new();
        scan(root, home, &HashSet::new(), &mut |b| out.extend(b));
        out
    }

    #[test]
    fn scan_walks_bfs_and_skips_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("work/proj/src")).unwrap();
        fs::create_dir_all(root.join("work/node_modules/dep")).unwrap();
        fs::create_dir_all(root.join(".hidden/inner")).unwrap();
        fs::create_dir_all(root.join("build-out/deep")).unwrap();
        fs::write(root.join("build-out/CACHEDIR.TAG"), "x").unwrap();
        fs::write(root.join("work/file.txt"), "x").unwrap();

        let got = collect(root, root);
        assert_eq!(got, vec!["~/work", "~/work/proj", "~/work/proj/src"]);
    }

    #[test]
    fn scan_skips_seeded_paths_but_descends_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        let skip: HashSet<String> = ["~/a".to_string()].into();
        let mut out = Vec::new();
        scan(root, root, &skip, &mut |b| out.extend(b));
        assert_eq!(out, vec!["~/a/b"]);
    }

    #[test]
    fn abbrev_home_variants() {
        let home = Path::new("/Users/me");
        assert_eq!(abbrev_home(Path::new("/Users/me/x"), home), "~/x");
        assert_eq!(abbrev_home(Path::new("/Users/me"), home), "~");
        assert_eq!(abbrev_home(Path::new("/srv/data"), home), "/srv/data");
    }
}
