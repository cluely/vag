//! Git diff computation for the per-session diff view.
//!
//! Everything here shells out to the real `git` and parses its output —
//! never libgit2 — so vag sees exactly what the user's git sees (same
//! config, same attributes, same rename detection). All functions are
//! synchronous and intended to run on the diff worker thread, never on the
//! UI thread.
//!
//! The displayed diff is always the live worktree state (`git diff <base>`,
//! optionally scoped by pathspec). Recorded transcript patches are never
//! rendered as content: measured on real stores, ~1/3 of recorded hunks no
//! longer describe the file on disk by review time.

use std::collections::BTreeSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Upper bound on the raw `git diff` text vag will parse. Beyond this the
/// snapshot is truncated at a file boundary and flagged, keeping the UI
/// responsive on pathological diffs (vendored trees, lockfile churn).
const MAX_DIFF_BYTES: usize = 8 * 1024 * 1024;
/// Per-file cap when synthesizing a diff for an untracked file.
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024;
/// A NUL byte in this prefix classifies an untracked file as binary — the
/// same heuristic git itself uses.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Git's well-known empty tree object. Sessions spawned on an unborn HEAD
/// (fresh `git init`) anchor here so staged files and the agent's own first
/// commits stay visible in the diff — with no base at all, `git diff`
/// degrades to worktree-vs-index and committed work silently vanishes.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    /// Not known to git yet; content synthesized as all-additions.
    Untracked,
}

impl FileStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Untracked => "?",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Remove,
    /// `\ No newline at end of file` and truncation markers.
    Meta,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number in the base version (None for Add/Meta).
    pub old_no: Option<u32>,
    /// Line number in the worktree version (None for Remove/Meta).
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    /// The full `@@ -a,b +c,d @@ context` header line.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Repo-relative path (new side for renames).
    pub path: String,
    /// Repo-relative pre-rename path, only for `Renamed`.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub adds: u32,
    pub dels: u32,
    pub hunks: Vec<Hunk>,
    /// This file's verbatim section of the git diff (synthesized for
    /// untracked files) — the input handed to an external renderer (delta).
    /// Bounded by [`MAX_DIFF_BYTES`] like everything else here.
    pub raw: String,
}

#[derive(Debug, Clone, Default)]
pub struct DiffSnapshot {
    /// The base actually diffed against (resolved sha), if any.
    pub base: Option<String>,
    /// Human note when the base was re-anchored (rebase, gc'd sha, …).
    pub base_note: Option<String>,
    pub files: Vec<FileDiff>,
    pub total_adds: u32,
    pub total_dels: u32,
    /// True when the raw diff exceeded [`MAX_DIFF_BYTES`] and later files
    /// were dropped. The UI must say so rather than imply completeness.
    pub truncated: bool,
    /// Cheap change detector: identical fingerprint ⇒ identical snapshot,
    /// so the UI can keep scroll/selection state without re-rendering.
    pub fingerprint: u64,
}

/// Run git with vag's hygiene defaults. `-C root` instead of `current_dir`
/// so error messages name the repo; `GIT_OPTIONAL_LOCKS=0` so a diff never
/// contends with the user's (or an agent's) in-flight index operations;
/// `core.quotepath=false` so non-ASCII paths arrive as UTF-8, not octal.
fn git(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("running git (is git installed?)")
}

fn git_ok(root: &Path, args: &[&str]) -> bool {
    git(root, args).map(|o| o.status.success()).unwrap_or(false)
}

fn git_stdout(root: &Path, args: &[&str]) -> Result<String> {
    let out = git(root, args)?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The repo containing `cwd`, or None when `cwd` isn't inside a work tree.
pub fn repo_root_for(cwd: &Path) -> Option<PathBuf> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Current HEAD sha, or None on unborn branches / non-repos.
pub fn head_sha(root: &Path) -> Option<String> {
    let out = git(root, &["rev-parse", "HEAD"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Resolve the recorded base into one that is safe to diff against.
///
/// A base that is no longer an ancestor of HEAD (user rebased/pulled) would
/// attribute the whole upstream delta to the session, so it is re-anchored
/// to `merge-base(recorded, HEAD)`; a base git no longer knows about (gc
/// after rewrite) falls back to HEAD. The note is surfaced verbatim in the
/// diff header so re-anchoring is never silent.
pub fn resolve_base(root: &Path, recorded: Option<&str>) -> (Option<String>, Option<String>) {
    let head = head_sha(root);
    let Some(recorded) = recorded else {
        return (head, None);
    };
    // The empty tree is a tree, not a commit: the `^{commit}` probe and the
    // merge-base ancestry checks below would both reject it and re-anchor
    // to HEAD — exactly what an unborn-HEAD session must not do.
    if recorded == EMPTY_TREE {
        return (Some(recorded.to_string()), None);
    }
    let known = git_ok(root, &["cat-file", "-e", &format!("{recorded}^{{commit}}")]);
    if !known {
        return (head, Some("recorded base commit is gone; showing changes vs HEAD".into()));
    }
    let Some(head) = head else {
        // Unborn HEAD with a recorded base can't happen in practice; diff
        // against the recorded commit anyway.
        return (Some(recorded.to_string()), None);
    };
    if git_ok(root, &["merge-base", "--is-ancestor", recorded, &head]) {
        return (Some(recorded.to_string()), None);
    }
    match git_stdout(root, &["merge-base", recorded, &head]) {
        Ok(mb) => {
            let mb = mb.trim().to_string();
            if mb.is_empty() {
                (Some(head), Some("history rewritten since session start; showing changes vs HEAD".into()))
            } else {
                (
                    Some(mb),
                    Some("history rewritten since session start; base re-anchored to merge-base".into()),
                )
            }
        }
        Err(_) => (
            Some(head),
            Some("history rewritten since session start; showing changes vs HEAD".into()),
        ),
    }
}

/// Compute the diff of the worktree against `base` (staged + unstaged),
/// optionally scoped to `paths` (absolute; entries outside the repo are
/// ignored). `paths = None` means the whole repo. Untracked files are
/// included as synthesized all-addition diffs when `include_untracked`.
pub fn compute_diff(
    root: &Path,
    base: Option<&str>,
    paths: Option<&[PathBuf]>,
    include_untracked: bool,
) -> Result<DiffSnapshot> {
    let pathspec: Option<Vec<String>> = paths.map(|ps| {
        let mut rel: BTreeSet<String> = ps
            .iter()
            .filter_map(|p| p.strip_prefix(root).ok())
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        expand_renames(root, base, &mut rel);
        rel.into_iter()
            .map(|r| format!(":(literal){r}"))
            .collect()
    });
    // Scoped to zero in-repo paths: an honest empty snapshot, not the whole
    // repo (the caller's scope toggle handles "show me everything").
    if matches!(&pathspec, Some(v) if v.is_empty()) {
        return Ok(DiffSnapshot::default());
    }

    let mut args: Vec<String> = vec![
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--find-renames".into(),
        "--ignore-submodules=dirty".into(),
    ];
    if let Some(base) = base {
        args.push(base.to_string());
    }
    if let Some(spec) = &pathspec {
        args.push("--".into());
        args.extend(spec.iter().cloned());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let raw = git_stdout(root, &arg_refs)?;

    let (mut files, truncated) = parse_unified(&raw);

    if include_untracked {
        files.extend(untracked_diffs(root, paths)?);
    }

    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    // Untracked content isn't in `raw` — hash the synthesized lines too,
    // or an edit that keeps an untracked file's line count would render as
    // "unchanged" and the view would go stale.
    for f in &files {
        if f.status == FileStatus::Untracked {
            f.path.hash(&mut hasher);
            for h in &f.hunks {
                for l in &h.lines {
                    l.text.hash(&mut hasher);
                }
            }
        }
    }

    let total_adds = files.iter().map(|f| f.adds).sum();
    let total_dels = files.iter().map(|f| f.dels).sum();
    Ok(DiffSnapshot {
        base: base.map(str::to_string),
        base_note: None,
        files,
        total_adds,
        total_dels,
        truncated,
        fingerprint: hasher.finish(),
    })
}

/// Rename detection needs BOTH sides of a rename inside the pathspec; the
/// transcript usually records only one (Claude edits the destination path,
/// Codex patches record the source). A cheap unscoped `--name-status`
/// pre-pass pairs them up, so a scoped diff shows the real `R +1 −1`
/// instead of a full-file addition that contradicts the all-files view.
/// Any failure leaves the scope untouched — degraded, never wrong content.
fn expand_renames(root: &Path, base: Option<&str>, rel: &mut BTreeSet<String>) {
    if rel.is_empty() {
        return;
    }
    let mut args: Vec<&str> = vec!["diff", "--name-status", "--find-renames", "-z"];
    if let Some(base) = base {
        args.push(base);
    }
    let Ok(out) = git_stdout(root, &args) else {
        return;
    };
    // -z framing: STATUS NUL PATH NUL, with a second path for R/C entries.
    let mut it = out.split('\0');
    while let Some(status) = it.next() {
        if status.is_empty() {
            break;
        }
        let two_paths = status.starts_with('R') || status.starts_with('C');
        let Some(first) = it.next() else { break };
        if two_paths {
            let Some(second) = it.next() else { break };
            if rel.contains(first) || rel.contains(second) {
                rel.insert(first.to_string());
                rel.insert(second.to_string());
            }
        }
    }
}

/// Decode one path token from a diff header line: strip the TAB separator
/// git appends after paths containing spaces, then C-unquote (git quotes
/// paths holding quotes/backslashes/control bytes even with quotepath off).
/// Malformed escapes return the token verbatim — showing an odd name beats
/// dropping the file.
fn decode_header_path(raw: &str) -> String {
    let raw = raw.strip_suffix('\t').unwrap_or(raw);
    let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
    else {
        return raw.to_string();
    };
    let mut bytes: Vec<u8> = Vec::with_capacity(inner.len());
    let mut it = inner.bytes();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(b'\\') => bytes.push(b'\\'),
            Some(b'"') => bytes.push(b'"'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'n') => bytes.push(b'\n'),
            Some(b'r') => bytes.push(b'\r'),
            Some(d @ b'0'..=b'7') => {
                let mut v = (d - b'0') as u32;
                for _ in 0..2 {
                    match it.next() {
                        Some(o @ b'0'..=b'7') => v = v * 8 + (o - b'0') as u32,
                        _ => return raw.to_string(),
                    }
                }
                bytes.push(v as u8);
            }
            _ => return raw.to_string(),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parse `git diff` unified output into per-file structures. Returns the
/// files plus whether the input was truncated at [`MAX_DIFF_BYTES`].
///
/// Path recovery deliberately avoids the `diff --git a/x b/y` line (spaces
/// make it ambiguous); `---`/`+++` lines are unambiguous, and pure renames
/// (no content change) fall back to `rename from`/`rename to`.
fn parse_unified(raw: &str) -> (Vec<FileDiff>, bool) {
    let (raw, truncated) = if raw.len() > MAX_DIFF_BYTES {
        // The cap can land mid-codepoint (diffs carry arbitrary UTF-8):
        // back up to a char boundary before slicing, or this panics.
        let mut cut = MAX_DIFF_BYTES;
        while cut > 0 && !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        // Cut at the last complete file section before the cap.
        let head = &raw[..cut];
        match head.rfind("\ndiff --git ") {
            Some(pos) => (&raw[..pos + 1], true),
            None => (head, true),
        }
    } else {
        (raw, false)
    };

    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur: Option<FileDiff> = None;
    // (old_no, new_no) counters inside the current hunk.
    let mut counters: Option<(u32, u32)> = None;

    for inc in raw.split_inclusive('\n') {
        let line = inc.strip_suffix('\n').unwrap_or(inc);
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(f) = cur.take() {
                files.push(f);
            }
            counters = None;
            cur = Some(FileDiff {
                // Placeholder; refined by ---/+++/rename lines below. The
                // `b/...` half is a last resort for exotic sections that
                // carry none of those (e.g. mode-only changes).
                path: rest
                    .rsplit_once(" b/")
                    .map(|(_, b)| b.to_string())
                    .unwrap_or_else(|| rest.to_string()),
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                adds: 0,
                dels: 0,
                hunks: Vec::new(),
                raw: inc.to_string(),
            });
            continue;
        }
        let Some(f) = cur.as_mut() else { continue };
        f.raw.push_str(inc);

        if counters.is_none() {
            // File header region.
            if line.starts_with("new file mode") {
                f.status = FileStatus::Added;
            } else if line.starts_with("deleted file mode") {
                f.status = FileStatus::Deleted;
            } else if let Some(p) = line.strip_prefix("rename from ") {
                f.status = FileStatus::Renamed;
                f.old_path = Some(decode_header_path(p));
            } else if let Some(p) = line.strip_prefix("rename to ") {
                f.status = FileStatus::Renamed;
                f.path = decode_header_path(p);
            } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
                f.binary = true;
            } else if let Some(p) = line.strip_prefix("--- ") {
                if p != "/dev/null" {
                    let p = decode_header_path(p);
                    let p = p.strip_prefix("a/").unwrap_or(&p);
                    if f.status == FileStatus::Renamed {
                        f.old_path = Some(p.to_string());
                    } else {
                        f.path = p.to_string();
                    }
                }
            } else if let Some(p) = line.strip_prefix("+++ ")
                && p != "/dev/null"
            {
                let p = decode_header_path(p);
                f.path = p.strip_prefix("b/").unwrap_or(&p).to_string();
            }
        }

        if let Some(hdr) = line.strip_prefix("@@") {
            let (old_start, new_start) = parse_hunk_header(hdr);
            counters = Some((old_start, new_start));
            f.hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }

        let Some((old_no, new_no)) = counters.as_mut() else {
            continue;
        };
        let Some(hunk) = f.hunks.last_mut() else {
            continue;
        };
        match line.as_bytes().first() {
            Some(b'+') => {
                f.adds += 1;
                hunk.lines.push(DiffLine {
                    kind: LineKind::Add,
                    old_no: None,
                    new_no: Some(*new_no),
                    text: line[1..].to_string(),
                });
                *new_no += 1;
            }
            Some(b'-') => {
                f.dels += 1;
                hunk.lines.push(DiffLine {
                    kind: LineKind::Remove,
                    old_no: Some(*old_no),
                    new_no: None,
                    text: line[1..].to_string(),
                });
                *old_no += 1;
            }
            Some(b' ') => {
                hunk.lines.push(DiffLine {
                    kind: LineKind::Context,
                    old_no: Some(*old_no),
                    new_no: Some(*new_no),
                    text: line[1..].to_string(),
                });
                *old_no += 1;
                *new_no += 1;
            }
            Some(b'\\') => {
                hunk.lines.push(DiffLine {
                    kind: LineKind::Meta,
                    old_no: None,
                    new_no: None,
                    text: line.to_string(),
                });
            }
            // Blank line inside a hunk is a context line whose content is
            // empty (git always prefixes, but be lenient).
            None => {
                hunk.lines.push(DiffLine {
                    kind: LineKind::Context,
                    old_no: Some(*old_no),
                    new_no: Some(*new_no),
                    text: String::new(),
                });
                *old_no += 1;
                *new_no += 1;
            }
            _ => {}
        }
    }
    if let Some(f) = cur.take() {
        files.push(f);
    }
    if truncated {
        files.push(FileDiff {
            path: "… diff truncated (too large)".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            adds: 0,
            dels: 0,
            hunks: Vec::new(),
            raw: String::new(),
        });
    }
    (files, truncated)
}

/// `-a[,b] +c[,d] @@ …` → (a, c). Malformed headers count from 1 rather
/// than failing: line numbers degrade, content still renders.
fn parse_hunk_header(rest: &str) -> (u32, u32) {
    let mut old_start = 1;
    let mut new_start = 1;
    for tok in rest.split_whitespace().take(2) {
        let (sign, body) = match tok.split_at_checked(1) {
            Some(x) => x,
            None => continue,
        };
        let num = body.split(',').next().unwrap_or("");
        let Ok(n) = num.parse::<u32>() else { continue };
        match sign {
            "-" => old_start = n,
            "+" => new_start = n,
            _ => {}
        }
    }
    (old_start, new_start)
}

/// Untracked files as synthesized all-addition diffs, so brand-new files an
/// agent created show up without vag ever touching the user's index.
fn untracked_diffs(root: &Path, paths: Option<&[PathBuf]>) -> Result<Vec<FileDiff>> {
    let mut args: Vec<String> = vec![
        "ls-files".into(),
        "--others".into(),
        "--exclude-standard".into(),
        "-z".into(),
    ];
    if let Some(ps) = paths {
        let spec: Vec<String> = ps
            .iter()
            .filter_map(|p| p.strip_prefix(root).ok())
            .map(|rel| format!(":(literal){}", rel.to_string_lossy()))
            .collect();
        if spec.is_empty() {
            return Ok(Vec::new());
        }
        args.push("--".into());
        args.extend(spec);
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git_stdout(root, &arg_refs)?;

    let mut files = Vec::new();
    for rel in out.split('\0').filter(|s| !s.is_empty()) {
        files.push(synthesize_untracked(root, rel));
    }
    Ok(files)
}

fn synthesize_untracked(root: &Path, rel: &str) -> FileDiff {
    let mut f = FileDiff {
        path: rel.to_string(),
        old_path: None,
        status: FileStatus::Untracked,
        binary: false,
        adds: 0,
        dels: 0,
        hunks: Vec::new(),
        raw: String::new(),
    };
    let abs = root.join(rel);
    let (bytes, clipped) = match std::fs::metadata(&abs) {
        Ok(md) if md.len() > MAX_UNTRACKED_BYTES => {
            let mut buf = vec![0_u8; MAX_UNTRACKED_BYTES as usize];
            match std::fs::File::open(&abs).and_then(|mut fh| {
                use std::io::Read;
                fh.read_exact(&mut buf).map(|_| buf)
            }) {
                Ok(b) => (b, true),
                Err(_) => return f,
            }
        }
        Ok(_) => match std::fs::read(&abs) {
            Ok(b) => (b, false),
            Err(_) => return f,
        },
        Err(_) => return f,
    };
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        f.binary = true;
        f.raw = format!(
            "diff --git a/{rel} b/{rel}\nnew file mode 100644\nBinary files /dev/null and b/{rel} differ\n"
        );
        return f;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<DiffLine> = Vec::new();
    for (n, l) in text.split_inclusive('\n').enumerate() {
        let l = l.strip_suffix('\n').unwrap_or(l);
        lines.push(DiffLine {
            kind: LineKind::Add,
            old_no: None,
            new_no: Some(n as u32 + 1),
            text: l.to_string(),
        });
    }
    f.adds = lines.len() as u32;
    // A synthesized-but-valid unified diff, so external renderers (delta)
    // can display untracked files exactly like tracked ones.
    if !lines.is_empty() {
        f.raw = format!(
            "diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{} @@\n",
            f.adds
        );
        for l in &lines {
            f.raw.push('+');
            f.raw.push_str(&l.text);
            f.raw.push('\n');
        }
    }
    if clipped {
        lines.push(DiffLine {
            kind: LineKind::Meta,
            old_no: None,
            new_no: None,
            text: "… file truncated (too large)".into(),
        });
    }
    if !lines.is_empty() {
        f.hunks.push(Hunk {
            header: format!("@@ -0,0 +1,{} @@", f.adds),
            lines,
        });
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,5 @@ mod header
 use std::fmt;
-fn old() {}
+fn new_one() {}
+fn extra() {}
 fn keep() {}
@@ -10,2 +11,2 @@
 tail
-x
+y
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3333333..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
diff --git a/new dir/with space.txt b/new dir/with space.txt
new file mode 100644
index 0000000..4444444
--- /dev/null
+++ b/new dir/with space.txt\t
@@ -0,0 +1,1 @@
+hello
diff --git a/old.rs b/renamed.rs
similarity index 90%
rename from old.rs
rename to renamed.rs
index 5555555..6666666 100644
--- a/old.rs
+++ b/renamed.rs
@@ -3,1 +3,1 @@
-a
+b
diff --git a/img.png b/img.png
index 7777777..8888888 100644
Binary files a/img.png and b/img.png differ
";

    #[test]
    fn parses_statuses_paths_and_counts() {
        let (files, truncated) = parse_unified(SAMPLE);
        assert!(!truncated);
        assert_eq!(files.len(), 5);

        let f = &files[0];
        assert_eq!(f.path, "src/lib.rs");
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!((f.adds, f.dels), (3, 2));
        assert_eq!(f.hunks.len(), 2);

        assert_eq!(files[1].status, FileStatus::Deleted);
        assert_eq!(files[1].path, "gone.txt");

        assert_eq!(files[2].status, FileStatus::Added);
        assert_eq!(files[2].path, "new dir/with space.txt");

        assert_eq!(files[3].status, FileStatus::Renamed);
        assert_eq!(files[3].path, "renamed.rs");
        assert_eq!(files[3].old_path.as_deref(), Some("old.rs"));

        assert!(files[4].binary);
        assert_eq!(files[4].path, "img.png");
        assert!(files[4].hunks.is_empty());
    }

    #[test]
    fn line_numbers_advance_correctly() {
        let (files, _) = parse_unified(SAMPLE);
        let h = &files[0].hunks[0];
        // @@ -1,4 +1,5 @@: ctx(1,1) del(2,-) add(-,2) add(-,3) ctx(3,4)
        let nums: Vec<(Option<u32>, Option<u32>)> =
            h.lines.iter().map(|l| (l.old_no, l.new_no)).collect();
        assert_eq!(
            nums,
            vec![
                (Some(1), Some(1)),
                (Some(2), None),
                (None, Some(2)),
                (None, Some(3)),
                (Some(3), Some(4)),
            ]
        );
        let meta = files[0].hunks[1].lines.last().unwrap();
        assert_eq!(meta.kind, LineKind::Meta);
        assert!(meta.text.starts_with('\\'));
    }

    #[test]
    fn header_paths_decode_tabs_and_c_quoting() {
        // Real git output: TAB separator after spaced paths, C-quoting for
        // quotes/backslashes/control bytes (even with core.quotepath=false).
        assert_eq!(decode_header_path("a/plain.rs"), "a/plain.rs");
        assert_eq!(decode_header_path("b/with space.txt\t"), "b/with space.txt");
        assert_eq!(
            decode_header_path("\"b/we\\\"ird\\\\name.txt\""),
            "b/we\"ird\\name.txt"
        );
        assert_eq!(decode_header_path("\"b/bell\\007.txt\""), "b/bell\u{7}.txt");
        // Malformed escapes fall back to the raw token, never panic/drop.
        assert_eq!(decode_header_path("\"b/bad\\q\""), "\"b/bad\\q\"");
    }

    #[test]
    fn hunk_header_parses_with_and_without_counts() {
        assert_eq!(parse_hunk_header(" -1,4 +1,5 @@"), (1, 1));
        assert_eq!(parse_hunk_header(" -10,2 +11,2 @@"), (10, 11));
        assert_eq!(parse_hunk_header(" -7 +9 @@"), (7, 9));
        assert_eq!(parse_hunk_header(" garbage"), (1, 1));
    }

    #[test]
    fn truncation_survives_multibyte_content_at_the_cap() {
        // One huge file (no later file boundary) of 4-byte emoji so the
        // byte cap lands mid-codepoint: must truncate, not panic.
        let mut big = String::from("diff --git a/e.txt b/e.txt\n--- a/e.txt\n+++ b/e.txt\n@@ -0,0 +1,999999 @@\n");
        while big.len() <= MAX_DIFF_BYTES + 8 {
            big.push_str("+😀😀😀😀😀😀😀😀😀😀\n");
        }
        let (files, truncated) = parse_unified(&big);
        assert!(truncated);
        assert_eq!(files[0].path, "e.txt");
    }

    #[test]
    fn truncation_cuts_at_file_boundary_and_flags() {
        let mut big = String::from("diff --git a/first.txt b/first.txt\n--- a/first.txt\n+++ b/first.txt\n@@ -1,1 +1,1 @@\n-a\n+b\n");
        let filler = "+x\n".repeat(MAX_DIFF_BYTES / 3);
        big.push_str("diff --git a/huge.txt b/huge.txt\n--- a/huge.txt\n+++ b/huge.txt\n@@ -1,1 +1,999999 @@\n");
        big.push_str(&filler);
        big.push_str(&filler);
        big.push_str(&filler);
        let (files, truncated) = parse_unified(&big);
        assert!(truncated);
        // first file intact; huge file dropped at the boundary; marker row added.
        assert_eq!(files[0].path, "first.txt");
        assert!(files.last().unwrap().path.contains("truncated"));
    }

    /// End-to-end against a real temporary repo: statuses, scoping,
    /// untracked synthesis, base resolution after a commit.
    #[test]
    fn integration_temp_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "--initial-branch=main"]);
        std::fs::write(root.join("kept.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("edited.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(root.join("doomed.txt"), "bye\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "base"]);
        let base = head_sha(root).unwrap();

        std::fs::write(root.join("edited.txt"), "alpha\nBETA\n").unwrap();
        std::fs::remove_file(root.join("doomed.txt")).unwrap();
        std::fs::write(root.join("fresh.txt"), "hi\n").unwrap();

        assert_eq!(repo_root_for(&root.join(".")).unwrap().canonicalize().unwrap(), root.canonicalize().unwrap());

        let snap = compute_diff(root, Some(&base), None, true).unwrap();
        let by_path = |p: &str| snap.files.iter().find(|f| f.path == p).unwrap();
        assert_eq!(by_path("edited.txt").status, FileStatus::Modified);
        assert_eq!(by_path("doomed.txt").status, FileStatus::Deleted);
        assert_eq!(by_path("fresh.txt").status, FileStatus::Untracked);
        assert_eq!(by_path("fresh.txt").adds, 1);
        assert!(snap.files.iter().all(|f| f.path != "kept.txt"));

        // Scoped: only the touched file, absolute paths, outsiders ignored.
        let scoped = compute_diff(
            root,
            Some(&base),
            Some(&[root.join("edited.txt"), PathBuf::from("/nowhere/else.txt")]),
            true,
        )
        .unwrap();
        assert_eq!(scoped.files.len(), 1);
        assert_eq!(scoped.files[0].path, "edited.txt");

        // Scoping to zero in-repo paths is an honest empty snapshot.
        let empty = compute_diff(root, Some(&base), Some(&[PathBuf::from("/nowhere")]), true).unwrap();
        assert!(empty.files.is_empty());

        // Fingerprint is stable across identical recomputes and changes on edit.
        let again = compute_diff(root, Some(&base), None, true).unwrap();
        assert_eq!(snap.fingerprint, again.fingerprint);
        std::fs::write(root.join("edited.txt"), "alpha\nGAMMA\n").unwrap();
        let changed = compute_diff(root, Some(&base), None, true).unwrap();
        assert_ne!(snap.fingerprint, changed.fingerprint);

        // Base resolution: ancestor base is kept; after a new commit the
        // old base still resolves (still an ancestor).
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "second"]);
        let (resolved, note) = resolve_base(root, Some(&base));
        assert_eq!(resolved.as_deref(), Some(base.as_str()));
        assert!(note.is_none());
        let (resolved, note) = resolve_base(root, Some("0000000000000000000000000000000000000000"));
        assert_eq!(resolved, head_sha(root));
        assert!(note.is_some());
    }

    /// The two review-found scope/base cases: a rename scoped to only one
    /// side must still render as a rename, and an unborn-HEAD repo anchors
    /// to the empty tree so staged/committed work is visible.
    #[test]
    fn scoped_rename_pairs_and_unborn_head_uses_empty_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "--initial-branch=main"]);

        // Unborn HEAD: a staged file diffs against the empty tree.
        std::fs::write(root.join("first.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        run(&["add", "."]);
        assert!(head_sha(root).is_none());
        let (resolved, note) = resolve_base(root, Some(EMPTY_TREE));
        assert_eq!(resolved.as_deref(), Some(EMPTY_TREE));
        assert!(note.is_none());
        let snap = compute_diff(root, Some(EMPTY_TREE), None, true).unwrap();
        assert_eq!(snap.files.len(), 1);
        assert_eq!(snap.files[0].status, FileStatus::Added);

        // Rename scoped to ONLY the destination path: the pre-pass must
        // pull in the source so git renders R, not a full-file A.
        run(&["commit", "-q", "-m", "base"]);
        let base = head_sha(root).unwrap();
        run(&["mv", "first.rs", "second.rs"]);
        std::fs::write(root.join("second.rs"), "fn a() {}\nfn c() {}\n").unwrap();
        run(&["add", "."]);
        let scoped =
            compute_diff(root, Some(&base), Some(&[root.join("second.rs")]), true).unwrap();
        assert_eq!(scoped.files.len(), 1);
        assert_eq!(scoped.files[0].status, FileStatus::Renamed);
        assert_eq!(scoped.files[0].old_path.as_deref(), Some("first.rs"));
        assert_eq!(scoped.files[0].path, "second.rs");
    }

    #[test]
    fn non_repo_paths_resolve_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(repo_root_for(tmp.path()).is_none());
        assert!(head_sha(tmp.path()).is_none());
    }
}
