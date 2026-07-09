//! vag's own organization state: the folder tree and the
//! `(agent, session_id) → folder` mapping. Stored as JSON at
//! `Config::data_dir()/state.json`, written atomically (temp file + rename in
//! the same directory).
//!
//! DESIGN INVARIANTS:
//! - Session files are NEVER moved on disk; this file is the only place
//!   folder membership lives, so a claude/codex update can't corrupt it.
//! - Sessions map is keyed by `SessionKey::to_key_string()` ("claude:<uuid>").
//! - Sessions absent from a scan are NOT dropped immediately: `gc_missing`
//!   stamps `missing_since` and only drops entries missing > 30 days
//!   (claude's cleanupPeriodDays deletes transcripts; a transient read error
//!   must not destroy organization).
//! - Folder ids are opaque short unique strings (uuid v4 simple/short form).
//!   Folder tree operations must reject cycles and dangling parents.
//! - Unknown JSON fields are preserved-on-read-if-possible or at minimum
//!   ignored without error (forward compat): use `#[serde(default)]`
//!   everywhere, never deny_unknown_fields.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::types::SessionKey;

/// Entries absent from a scan are kept this long before GC drops them.
const MISSING_GRACE_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Folder {
    pub id: String,
    pub name: String,
    /// None = top level.
    pub parent: Option<String>,
    /// Optional directory binding: "new session in this folder" starts here.
    pub default_dir: Option<PathBuf>,
    /// Project scoping: Some(repo root) = a PROJECT folder, always visible
    /// when that repo's filter is active (even empty) and labelled in the
    /// global view; None = a GLOBAL folder, visible unfiltered but hidden
    /// under a repo filter unless it holds in-scope sessions. Folders made
    /// while the g-filter is on get the current repo; subfolders inherit
    /// their parent's scope.
    pub scope: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionRef {
    /// Folder id; None = Inbox (ungrouped).
    pub folder: Option<String>,
    /// Set for sessions living on an SSH remote (the `[[remotes]]` name).
    /// Remote sessions are invisible to local scans: this state file is
    /// their source of truth, so gc must never drop them for being absent.
    pub remote: Option<String>,
    /// Working directory ON the remote (local sessions: None; their cwd
    /// comes from the scan).
    pub remote_cwd: Option<String>,
    /// User rename inside vag (overrides the agent-store title for display).
    pub name_override: Option<String>,
    /// Accent color for this session (palette name or "#rrggbb"): tints its
    /// tree row and the pane titlebar. None = default styling.
    pub color: Option<String>,
    pub hidden: bool,
    pub last_opened: Option<DateTime<Utc>>,
    /// Set when a scan no longer finds the session; cleared when it reappears.
    pub missing_since: Option<DateTime<Utc>>,
}

impl SessionRef {
    /// True if the user never touched this entry: it carries no information,
    /// so GC may drop it the moment the session disappears.
    fn is_default_uncustomized(&self) -> bool {
        self.remote.is_none()
            && self.folder.is_none()
            && self.name_override.is_none()
            && self.color.is_none()
            && !self.hidden
            && self.last_opened.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VagState {
    pub version: u32,
    pub folders: Vec<Folder>,
    /// Keyed by SessionKey::to_key_string().
    pub sessions: BTreeMap<String, SessionRef>,
}

impl Default for VagState {
    fn default() -> Self {
        VagState {
            version: 1,
            folders: vec![],
            sessions: BTreeMap::new(),
        }
    }
}

impl VagState {
    fn default_path() -> PathBuf {
        Config::data_dir().join("state.json")
    }

    /// Load from `Config::data_dir()/state.json`; missing file → default.
    /// Corrupt file → Err (the app surfaces it and refuses to overwrite).
    pub fn load() -> Result<VagState> {
        Self::load_from(&Self::default_path())
    }

    /// Load from an explicit path (for tests).
    pub fn load_from(path: &std::path::Path) -> Result<VagState> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(VagState::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading state at {}", path.display()));
            }
        };
        serde_json::from_str(&text).with_context(|| {
            format!(
                "corrupt state file at {} — refusing to overwrite it; fix or move it aside",
                path.display()
            )
        })
    }

    /// Atomic write (tmp + rename, same dir), creating parent dirs.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::default_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating state dir {}", dir.display()))?;
        }
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow!("state path has no file name: {}", path.display()))?;
        let mut tmp_name = file_name.to_os_string();
        tmp_name.push(format!(".tmp-{}", std::process::id()));
        let tmp = path.with_file_name(tmp_name);

        let json = serde_json::to_string_pretty(self).context("serializing state")?;
        let write_and_rename = (|| -> Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        if write_and_rename.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        write_and_rename.with_context(|| format!("writing state to {}", path.display()))
    }

    // ---- folders ----

    /// Create a GLOBAL folder, return its id. `parent` must exist (or be
    /// None). Subfolders still inherit a scoped parent's scope.
    #[allow(dead_code)] // convenience wrapper; the app always passes a view scope
    pub fn create_folder(&mut self, name: &str, parent: Option<&str>) -> Result<String> {
        self.create_folder_scoped(name, parent, None)
    }

    /// Create a folder scoped to a repo root (project folder) when `scope`
    /// is Some. A parent's scope always wins: subfolders of a project
    /// folder belong to that project no matter the current view.
    pub fn create_folder_scoped(
        &mut self,
        name: &str,
        parent: Option<&str>,
        scope: Option<PathBuf>,
    ) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            bail!("folder name cannot be empty");
        }
        let inherited = match parent {
            Some(p) => match self.folder(p) {
                Some(f) => f.scope.clone().or(scope),
                None => bail!("parent folder `{p}` does not exist"),
            },
            None => scope,
        };
        let id = self.fresh_folder_id();
        self.folders.push(Folder {
            id: id.clone(),
            name: name.to_string(),
            parent: parent.map(str::to_string),
            default_dir: None,
            scope: inherited,
        });
        Ok(id)
    }

    pub fn rename_folder(&mut self, id: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("folder name cannot be empty");
        }
        let f = self.folder_mut(id)?;
        f.name = name.to_string();
        Ok(())
    }

    /// Delete folder: child folders re-parent to the deleted folder's parent;
    /// member sessions move to the deleted folder's parent (or Inbox).
    pub fn delete_folder(&mut self, id: &str) -> Result<()> {
        let pos = self
            .folders
            .iter()
            .position(|f| f.id == id)
            .ok_or_else(|| anyhow!("folder `{id}` does not exist"))?;
        let removed = self.folders.remove(pos);
        for f in &mut self.folders {
            if f.parent.as_deref() == Some(id) {
                f.parent = removed.parent.clone();
            }
        }
        for s in self.sessions.values_mut() {
            if s.folder.as_deref() == Some(id) {
                s.folder = removed.parent.clone();
            }
        }
        Ok(())
    }

    /// Re-parent a folder. Rejects cycles and unknown ids.
    #[allow(dead_code)] // no folder-move keybinding yet
    pub fn move_folder(&mut self, id: &str, new_parent: Option<&str>) -> Result<()> {
        let current_parent = self
            .folder(id)
            .ok_or_else(|| anyhow!("folder `{id}` does not exist"))?
            .parent
            .clone();
        if current_parent.as_deref() == new_parent {
            return Ok(());
        }
        if let Some(np) = new_parent {
            if self.folder(np).is_none() {
                bail!("parent folder `{np}` does not exist");
            }
            // Walk np's ancestor chain; reaching `id` means the move would
            // create a cycle. Bounded so pre-existing corruption can't loop.
            let mut cursor = Some(np);
            for _ in 0..=self.folders.len() {
                let Some(c) = cursor else { break };
                if c == id {
                    bail!("cannot move folder `{id}` under its own descendant `{np}`");
                }
                cursor = self.folder(c).and_then(|f| f.parent.as_deref());
            }
        }
        self.folder_mut(id)?.parent = new_parent.map(str::to_string);
        Ok(())
    }

    pub fn set_folder_default_dir(&mut self, id: &str, dir: Option<PathBuf>) -> Result<()> {
        self.folder_mut(id)?.default_dir = dir;
        Ok(())
    }

    pub fn folder(&self, id: &str) -> Option<&Folder> {
        self.folders.iter().find(|f| f.id == id)
    }

    /// Direct children of `parent` (None = top level), sorted by name.
    pub fn children_of(&self, parent: Option<&str>) -> Vec<&Folder> {
        let mut children: Vec<&Folder> = self
            .folders
            .iter()
            .filter(|f| f.parent.as_deref() == parent)
            .collect();
        children.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        children
    }

    fn folder_mut(&mut self, id: &str) -> Result<&mut Folder> {
        self.folders
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| anyhow!("folder `{id}` does not exist"))
    }

    fn fresh_folder_id(&self) -> String {
        loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            if self.folder(&id).is_none() {
                return id;
            }
        }
    }

    // ---- sessions ----

    /// Get-or-create the entry for a session.
    pub fn session_mut(&mut self, key: &SessionKey) -> &mut SessionRef {
        self.sessions.entry(key.to_key_string()).or_default()
    }

    pub fn session(&self, key: &SessionKey) -> Option<&SessionRef> {
        self.sessions.get(&key.to_key_string())
    }

    /// Assign a session to a folder (None = Inbox). Unknown folder id → Err.
    pub fn set_session_folder(&mut self, key: &SessionKey, folder: Option<&str>) -> Result<()> {
        if let Some(f) = folder
            && self.folder(f).is_none()
        {
            bail!("folder `{f}` does not exist");
        }
        self.session_mut(key).folder = folder.map(str::to_string);
        Ok(())
    }

    /// Reconcile with a scan: `present` = key-strings seen this scan.
    /// Present entries get missing_since cleared; absent entries get it set
    /// (if unset); entries missing longer than 30 days are dropped.
    /// Note: entries with `remote` set are exempt — local scans can never
    /// see them, so absence means nothing.
    pub fn gc_missing(&mut self, present: &std::collections::HashSet<String>, now: DateTime<Utc>) {
        let grace = Duration::days(MISSING_GRACE_DAYS);
        self.sessions.retain(|key, entry| {
            // Remote sessions are never in a local scan; keep them always.
            if entry.remote.is_some() {
                entry.missing_since = None;
                return true;
            }
            if present.contains(key) {
                entry.missing_since = None;
                return true;
            }
            // Untouched entries carry no user data — drop right away.
            if entry.is_default_uncustomized() {
                return false;
            }
            match entry.missing_since {
                None => {
                    entry.missing_since = Some(now);
                    true
                }
                Some(since) => now - since <= grace,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentKind;
    use std::collections::HashSet;

    fn key(id: &str) -> SessionKey {
        SessionKey::new(AgentKind::Claude, id)
    }

    // ---- folder CRUD ----

    #[test]
    fn create_folder_basics() {
        let mut st = VagState::default();
        let a = st.create_folder("  work  ", None).unwrap();
        assert_eq!(a.len(), 8);
        assert_eq!(st.folder(&a).unwrap().name, "work");
        assert_eq!(st.folder(&a).unwrap().parent, None);

        let b = st.create_folder("sub", Some(&a)).unwrap();
        assert_ne!(a, b);
        assert_eq!(st.folder(&b).unwrap().parent.as_deref(), Some(a.as_str()));
    }

    #[test]
    fn create_folder_rejects_empty_name_and_dangling_parent() {
        let mut st = VagState::default();
        assert!(st.create_folder("", None).is_err());
        assert!(st.create_folder("   ", None).is_err());
        assert!(st.create_folder("x", Some("nope")).is_err());
        assert!(st.folders.is_empty());
    }

    #[test]
    fn rename_folder_works_and_validates() {
        let mut st = VagState::default();
        let a = st.create_folder("old", None).unwrap();
        st.rename_folder(&a, "  new  ").unwrap();
        assert_eq!(st.folder(&a).unwrap().name, "new");
        assert!(st.rename_folder(&a, "  ").is_err());
        assert!(st.rename_folder("nope", "x").is_err());
    }

    #[test]
    fn set_folder_default_dir_works() {
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        st.set_folder_default_dir(&a, Some(PathBuf::from("/tmp/proj")))
            .unwrap();
        assert_eq!(
            st.folder(&a).unwrap().default_dir.as_deref(),
            Some("/tmp/proj".as_ref())
        );
        st.set_folder_default_dir(&a, None).unwrap();
        assert_eq!(st.folder(&a).unwrap().default_dir, None);
        assert!(st.set_folder_default_dir("nope", None).is_err());
    }

    // ---- move_folder ----

    #[test]
    fn move_folder_rejects_cycles() {
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        let b = st.create_folder("b", Some(&a)).unwrap();
        let c = st.create_folder("c", Some(&b)).unwrap();

        assert!(st.move_folder(&a, Some(&c)).is_err()); // grandchild
        assert!(st.move_folder(&a, Some(&b)).is_err()); // child
        assert!(st.move_folder(&a, Some(&a)).is_err()); // itself
        assert_eq!(st.folder(&a).unwrap().parent, None); // unchanged
    }

    #[test]
    fn move_folder_rejects_unknown_ids() {
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        assert!(st.move_folder("nope", None).is_err());
        assert!(st.move_folder(&a, Some("nope")).is_err());
    }

    #[test]
    fn move_folder_to_current_parent_is_noop() {
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        let b = st.create_folder("b", Some(&a)).unwrap();
        st.move_folder(&b, Some(&a)).unwrap();
        st.move_folder(&a, None).unwrap();
        assert_eq!(st.folder(&b).unwrap().parent.as_deref(), Some(a.as_str()));
    }

    #[test]
    fn move_folder_reparents() {
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        let b = st.create_folder("b", None).unwrap();
        st.move_folder(&b, Some(&a)).unwrap();
        assert_eq!(st.folder(&b).unwrap().parent.as_deref(), Some(a.as_str()));
        st.move_folder(&b, None).unwrap();
        assert_eq!(st.folder(&b).unwrap().parent, None);
    }

    // ---- delete_folder ----

    #[test]
    fn delete_folder_reparents_children_and_sessions() {
        let mut st = VagState::default();
        let top = st.create_folder("top", None).unwrap();
        let mid = st.create_folder("mid", Some(&top)).unwrap();
        let leaf = st.create_folder("leaf", Some(&mid)).unwrap();
        st.set_session_folder(&key("s-mid"), Some(&mid)).unwrap();
        st.set_session_folder(&key("s-top"), Some(&top)).unwrap();

        st.delete_folder(&mid).unwrap();
        assert!(st.folder(&mid).is_none());
        assert_eq!(
            st.folder(&leaf).unwrap().parent.as_deref(),
            Some(top.as_str())
        );
        assert_eq!(
            st.session(&key("s-mid")).unwrap().folder.as_deref(),
            Some(top.as_str())
        );
        assert_eq!(
            st.session(&key("s-top")).unwrap().folder.as_deref(),
            Some(top.as_str())
        );
    }

    #[test]
    fn delete_top_level_folder_moves_members_to_inbox() {
        let mut st = VagState::default();
        let top = st.create_folder("top", None).unwrap();
        let child = st.create_folder("child", Some(&top)).unwrap();
        st.set_session_folder(&key("s1"), Some(&top)).unwrap();

        st.delete_folder(&top).unwrap();
        assert_eq!(st.folder(&child).unwrap().parent, None);
        assert_eq!(st.session(&key("s1")).unwrap().folder, None);
    }

    #[test]
    fn delete_unknown_folder_errs() {
        let mut st = VagState::default();
        assert!(st.delete_folder("nope").is_err());
    }

    // ---- children_of ----

    #[test]
    fn children_of_sorts_case_insensitively() {
        let mut st = VagState::default();
        st.create_folder("banana", None).unwrap();
        st.create_folder("Apple", None).unwrap();
        st.create_folder("cherry", None).unwrap();
        let top = st.create_folder("Zparent", None).unwrap();
        st.create_folder("inner", Some(&top)).unwrap();

        let names: Vec<&str> = st
            .children_of(None)
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, vec!["Apple", "banana", "cherry", "Zparent"]);
        let inner: Vec<&str> = st
            .children_of(Some(&top))
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(inner, vec!["inner"]);
    }

    // ---- sessions ----

    #[test]
    fn set_session_folder_validates_folder() {
        let mut st = VagState::default();
        assert!(st.set_session_folder(&key("s1"), Some("nope")).is_err());
        assert!(st.session(&key("s1")).is_none()); // failed set creates nothing

        let a = st.create_folder("a", None).unwrap();
        st.set_session_folder(&key("s1"), Some(&a)).unwrap();
        assert_eq!(
            st.session(&key("s1")).unwrap().folder.as_deref(),
            Some(a.as_str())
        );
        st.set_session_folder(&key("s1"), None).unwrap();
        assert_eq!(st.session(&key("s1")).unwrap().folder, None);
    }

    #[test]
    fn session_mut_creates_default_entry() {
        let mut st = VagState::default();
        st.session_mut(&key("s1")).hidden = true;
        assert!(st.session(&key("s1")).unwrap().hidden);
        assert!(st.sessions.contains_key("claude:s1"));
    }

    // ---- gc_missing ----

    #[test]
    fn gc_clears_missing_since_on_reappearance() {
        let now = Utc::now();
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        st.set_session_folder(&key("s1"), Some(&a)).unwrap();
        st.session_mut(&key("s1")).missing_since = Some(now - Duration::days(10));

        let present: HashSet<String> = [key("s1").to_key_string()].into();
        st.gc_missing(&present, now);
        assert_eq!(st.session(&key("s1")).unwrap().missing_since, None);
    }

    #[test]
    fn gc_customized_entry_survives_absence_within_grace() {
        let now = Utc::now();
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        st.set_session_folder(&key("s1"), Some(&a)).unwrap();

        // First absent scan: stamped, kept.
        st.gc_missing(&HashSet::new(), now);
        assert_eq!(st.session(&key("s1")).unwrap().missing_since, Some(now));

        // 29 days later, still absent: kept.
        st.gc_missing(&HashSet::new(), now + Duration::days(29));
        assert!(st.session(&key("s1")).is_some());

        // 31 days later: dropped.
        st.gc_missing(&HashSet::new(), now + Duration::days(31));
        assert!(st.session(&key("s1")).is_none());
    }

    #[test]
    fn gc_drops_uncustomized_entries_immediately() {
        let now = Utc::now();
        let mut st = VagState::default();
        st.session_mut(&key("untouched"));
        st.session_mut(&key("renamed")).name_override = Some("kept".into());
        st.session_mut(&key("hidden")).hidden = true;
        st.session_mut(&key("opened")).last_opened = Some(now);

        st.gc_missing(&HashSet::new(), now);
        assert!(st.session(&key("untouched")).is_none());
        assert!(st.session(&key("renamed")).is_some());
        assert!(st.session(&key("hidden")).is_some());
        assert!(st.session(&key("opened")).is_some());
    }

    #[test]
    fn gc_drops_entries_missing_longer_than_grace() {
        let now = Utc::now();
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        st.set_session_folder(&key("old"), Some(&a)).unwrap();
        st.session_mut(&key("old")).missing_since = Some(now - Duration::days(31));
        st.set_session_folder(&key("fresh"), Some(&a)).unwrap();
        st.session_mut(&key("fresh")).missing_since = Some(now - Duration::days(29));

        st.gc_missing(&HashSet::new(), now);
        assert!(st.session(&key("old")).is_none());
        assert!(st.session(&key("fresh")).is_some());
    }

    // ---- persistence ----

    #[test]
    fn save_load_roundtrip_preserves_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.json");

        let now = Utc::now();
        let mut st = VagState::default();
        let a = st.create_folder("a", None).unwrap();
        let b = st.create_folder("b", Some(&a)).unwrap();
        st.set_folder_default_dir(&b, Some(PathBuf::from("/tmp/proj")))
            .unwrap();
        st.set_session_folder(&key("s1"), Some(&b)).unwrap();
        let s = st.session_mut(&key("s1"));
        s.name_override = Some("my session".into());
        s.hidden = true;
        s.last_opened = Some(now);
        s.missing_since = Some(now - Duration::days(3));

        st.save_to(&path).unwrap();
        let loaded = VagState::load_from(&path).unwrap();

        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.folders.len(), 2);
        assert_eq!(
            loaded.folder(&b).unwrap().parent.as_deref(),
            Some(a.as_str())
        );
        assert_eq!(
            loaded.folder(&b).unwrap().default_dir.as_deref(),
            Some("/tmp/proj".as_ref())
        );
        let ls = loaded.session(&key("s1")).unwrap();
        assert_eq!(ls.folder.as_deref(), Some(b.as_str()));
        assert_eq!(ls.name_override.as_deref(), Some("my session"));
        assert!(ls.hidden);
        assert_eq!(ls.last_opened, Some(now));
        assert_eq!(ls.missing_since, Some(now - Duration::days(3)));
    }

    #[test]
    fn load_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let st = VagState::load_from(&dir.path().join("no-such.json")).unwrap();
        assert_eq!(st.version, 1);
        assert!(st.folders.is_empty());
        assert!(st.sessions.is_empty());
    }

    #[test]
    fn load_tolerates_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "future_top_level": {"x": 1},
                "folders": [
                    {"id": "f1", "name": "work", "parent": null, "future_field": true}
                ],
                "sessions": {
                    "claude:abc": {"folder": "f1", "future_nested": [1, 2]}
                }
            }"#,
        )
        .unwrap();
        let st = VagState::load_from(&path).unwrap();
        assert_eq!(st.folder("f1").unwrap().name, "work");
        assert_eq!(
            st.session(&key("abc")).unwrap().folder.as_deref(),
            Some("f1")
        );
    }

    #[test]
    fn load_missing_fields_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"sessions": {"codex:xyz": {}}}"#).unwrap();
        let st = VagState::load_from(&path).unwrap();
        let s = st
            .session(&SessionKey::new(AgentKind::Codex, "xyz"))
            .unwrap();
        assert_eq!(s.folder, None);
        assert!(!s.hidden);
    }

    #[test]
    fn load_corrupt_file_errs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ not json !!!").unwrap();
        let err = VagState::load_from(&path).unwrap_err();
        assert!(err.to_string().contains("state.json"));
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut st = VagState::default();
        st.create_folder("a", None).unwrap();
        st.save_to(&path).unwrap();
        st.save_to(&path).unwrap(); // overwrite path too

        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["state.json"]);
    }
}
