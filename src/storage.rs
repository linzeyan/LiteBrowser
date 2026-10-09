//! Bookmarks, history and the saved session, stored as small JSON files.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    crate::paths::write_atomic(path, &bytes)
}

/// Pages that are not worth remembering.
pub fn is_recordable(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file:")
}

// ---------------------------------------------------------------------------------------------
// Bookmarks

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub added: u64,
    /// Folder path like "工作/專案"; "" = directly on the bookmarks bar. A folder exists while
    /// it holds a bookmark, so there is nothing else to store for it.
    #[serde(default)]
    pub folder: String,
}

/// What a folder shows, in bookmark order: its own bookmarks, and each subfolder once, where its
/// first bookmark is.
#[derive(Debug, PartialEq)]
pub enum FolderEntry {
    Bookmark(usize),
    /// Full path and display name.
    Folder(String, String),
}

/// Where imports file what lived outside the source browser's bar. Like Chrome's "Other
/// bookmarks", the bookmarks bar keeps it at its right end.
pub const OTHER_FOLDER: &str = "其他書籤";

/// "a / b/" → "a/b".
fn normalize_folder(folder: &str) -> String {
    folder.split('/').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
}

pub struct Bookmarks {
    items: Vec<Bookmark>,
    path: PathBuf,
    dirty: bool,
}

impl Bookmarks {
    pub fn load(path: &Path) -> Self {
        Self { items: load_json(path), path: path.to_path_buf(), dirty: false }
    }

    pub fn items(&self) -> &[Bookmark] {
        &self.items
    }

    pub fn contains(&self, url: &str) -> bool {
        self.items.iter().any(|b| b.url == url)
    }

    /// Adds or removes `url` on the bookmarks bar; returns whether it is bookmarked afterwards.
    pub fn toggle(&mut self, url: &str, title: &str) -> bool {
        if let Some(pos) = self.items.iter().position(|b| b.url == url) {
            self.items.remove(pos);
            self.dirty = true;
            false
        } else {
            self.add(url, title, "");
            true
        }
    }

    pub fn add(&mut self, url: &str, title: &str, folder: &str) {
        let title = if title.trim().is_empty() { url } else { title };
        let folder = normalize_folder(folder);
        self.items.push(Bookmark { title: title.to_string(), url: url.to_string(), added: now_secs(), folder });
        self.dirty = true;
    }

    /// For a re-import: a bookmark still loose on the bar moves into the folder it has in the
    /// source browser (earlier versions imported everything flat onto the bar).
    pub fn file_if_loose(&mut self, url: &str, folder: &str) {
        let folder = normalize_folder(folder);
        if let Some(b) = self.items.iter_mut().find(|b| b.url == url && b.folder.is_empty() && !folder.is_empty()) {
            b.folder = folder;
            self.dirty = true;
        }
    }

    /// Edits a bookmark in place. An empty title falls back to the URL.
    pub fn update(&mut self, index: usize, title: &str, url: &str, folder: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        if let Some(b) = self.items.get_mut(index) {
            let title = title.trim();
            b.title = if title.is_empty() { url.to_string() } else { title.to_string() };
            b.url = url.to_string();
            b.folder = normalize_folder(folder);
            self.dirty = true;
        }
    }

    pub fn folder_entries(&self, folder: &str) -> Vec<FolderEntry> {
        let mut out = Vec::new();
        for (i, b) in self.items.iter().enumerate() {
            if b.folder == folder {
                out.push(FolderEntry::Bookmark(i));
                continue;
            }
            let rest = if folder.is_empty() { Some(b.folder.as_str()) } else { b.folder.strip_prefix(folder).and_then(|r| r.strip_prefix('/')) };
            let Some(name) = rest.and_then(|r| r.split('/').next()) else { continue };
            let path = if folder.is_empty() { name.to_string() } else { format!("{folder}/{name}") };
            if !out.iter().any(|e| matches!(e, FolderEntry::Folder(p, _) if *p == path)) {
                out.push(FolderEntry::Folder(path, name.to_string()));
            }
        }
        out
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.items.len() {
            self.items.remove(index);
            self.dirty = true;
        }
    }

    pub fn save_if_dirty(&mut self) {
        if self.dirty && save_json(&self.path, &self.items).is_ok() {
            self.dirty = false;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// History

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub url: String,
    pub title: String,
    pub last_visit: u64,
    pub visits: u32,
}

pub struct History {
    /// Most recent first.
    items: Vec<HistoryEntry>,
    path: PathBuf,
    dirty: bool,
    cap: usize,
}

impl History {
    pub fn load(path: &Path) -> Self {
        let mut items: Vec<HistoryEntry> = load_json(path);
        items.sort_by_key(|e| std::cmp::Reverse(e.last_visit));
        Self { items, path: path.to_path_buf(), dirty: false, cap: 5000 }
    }

    pub fn items(&self) -> &[HistoryEntry] {
        &self.items
    }

    pub fn record(&mut self, url: &str, title: &str, now: u64) {
        if !is_recordable(url) {
            return;
        }
        let mut entry = match self.items.iter().position(|e| e.url == url) {
            Some(pos) => self.items.remove(pos),
            None => HistoryEntry { url: url.to_string(), title: String::new(), last_visit: 0, visits: 0 },
        };
        entry.visits = entry.visits.saturating_add(1);
        entry.last_visit = now;
        if !title.is_empty() {
            entry.title = title.to_string();
        }
        self.items.insert(0, entry);
        self.items.truncate(self.cap);
        self.dirty = true;
    }

    pub fn update_title(&mut self, url: &str, title: &str) {
        if let Some(e) = self.items.iter_mut().find(|e| e.url == url) {
            if e.title != title && !title.is_empty() {
                e.title = title.to_string();
                self.dirty = true;
            }
        }
    }

    pub fn remove_url(&mut self, url: &str) {
        let before = self.items.len();
        self.items.retain(|e| e.url != url);
        self.dirty |= self.items.len() != before;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.dirty = true;
    }

    /// Entries whose title or URL contains every word of `query` (case-insensitive), most recent first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&HistoryEntry> {
        let words = query_words(query);
        self.items.iter().filter(|e| matches_words(&words, &e.title, &e.url)).take(limit).collect()
    }

    /// Most visited sites, for the new tab page.
    pub fn top_sites(&self, limit: usize) -> Vec<&HistoryEntry> {
        let mut all: Vec<&HistoryEntry> = self.items.iter().take(500).collect();
        all.sort_by(|a, b| b.visits.cmp(&a.visits).then(b.last_visit.cmp(&a.last_visit)));
        all.truncate(limit);
        all
    }

    pub fn save_if_dirty(&mut self) {
        if self.dirty && save_json(&self.path, &self.items).is_ok() {
            self.dirty = false;
        }
    }
}

fn query_words(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

fn matches_words(words: &[String], title: &str, url: &str) -> bool {
    let title = title.to_lowercase();
    let url = url.to_lowercase();
    words.iter().all(|w| title.contains(w.as_str()) || url.contains(w.as_str()))
}

// ---------------------------------------------------------------------------------------------
// Address bar suggestions

#[derive(Clone, Debug, PartialEq)]
pub enum SuggestionKind {
    Bookmark,
    History,
    Search,
    Url,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub kind: SuggestionKind,
    pub title: String,
    /// What to load when picked (already a URL).
    pub url: String,
}

pub fn suggestions(
    query: &str,
    bookmarks: &Bookmarks,
    history: &History,
    search_url: &str,
    limit: usize,
) -> Vec<Suggestion> {
    let mut out = Vec::new();
    if let Some(target) = crate::url_input::to_url(query, search_url) {
        let is_search = target == search_url.replace("{}", &crate::url_input::encode_query(query.trim()));
        out.push(Suggestion {
            kind: if is_search { SuggestionKind::Search } else { SuggestionKind::Url },
            title: if is_search { format!("搜尋「{}」", query.trim()) } else { target.clone() },
            url: target,
        });
    } else {
        return out;
    }
    let words = query_words(query);
    for b in bookmarks.items().iter().filter(|b| matches_words(&words, &b.title, &b.url)) {
        if out.len() > limit / 2 {
            break;
        }
        if !out.iter().any(|s| s.url == b.url) {
            out.push(Suggestion { kind: SuggestionKind::Bookmark, title: b.title.clone(), url: b.url.clone() });
        }
    }
    let mut hist = history.search(query, limit * 4);
    hist.sort_by(|a, b| b.visits.cmp(&a.visits).then(b.last_visit.cmp(&a.last_visit)));
    for h in hist {
        if out.len() >= limit {
            break;
        }
        if !out.iter().any(|s| s.url == h.url) {
            let title = if h.title.is_empty() { h.url.clone() } else { h.title.clone() };
            out.push(Suggestion { kind: SuggestionKind::History, title, url: h.url.clone() });
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Session

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionTab {
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// Logical size of the window when it was last not maximized.
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub tabs: Vec<SessionTab>,
    pub active: usize,
    #[serde(default)]
    pub window: Option<WindowState>,
}

impl Session {
    pub fn load(path: &Path) -> Self {
        load_json(path)
    }

    pub fn save(&self, path: &Path) {
        let _ = save_json(path, self);
    }
}

/// "3 分鐘前" style relative time.
pub fn relative_time(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=59 => "剛剛".into(),
        60..=3599 => format!("{} 分鐘前", d / 60),
        3600..=86_399 => format!("{} 小時前", d / 3600),
        _ => format!("{} 天前", d / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("litebrowser-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bookmarks_toggle_and_persist() {
        let dir = temp_dir("bm");
        let path = dir.join("b.json");
        let mut b = Bookmarks::load(&path);
        assert!(b.toggle("https://github.com", "GitHub"));
        assert!(b.contains("https://github.com"));
        b.save_if_dirty();
        let mut b2 = Bookmarks::load(&path);
        assert_eq!(b2.items().len(), 1);
        assert!(!b2.toggle("https://github.com", ""));
        assert!(b2.items().is_empty());
    }

    #[test]
    fn bookmarks_can_be_edited() {
        let dir = temp_dir("bm-edit");
        let path = dir.join("b.json");
        let mut b = Bookmarks::load(&path);
        b.toggle("https://a.com", "A");
        b.update(0, "  Renamed  ", " https://b.com ", " 工作 / 專案/ ");
        assert_eq!(b.items()[0].title, "Renamed");
        assert_eq!(b.items()[0].url, "https://b.com");
        assert_eq!(b.items()[0].folder, "工作/專案");
        // An empty title falls back to the URL; an empty URL is rejected.
        b.update(0, "", "https://c.com", "");
        assert_eq!(b.items()[0].title, "https://c.com");
        assert_eq!(b.items()[0].folder, "", "an empty folder puts it back on the bar");
        b.update(0, "keep", "   ", "");
        assert_eq!(b.items()[0].url, "https://c.com");
        b.update(9, "x", "https://x.com", ""); // out of range is a no-op
        assert_eq!(b.items().len(), 1);
    }

    #[test]
    fn folders_show_where_their_first_bookmark_is() {
        let dir = temp_dir("bm-folders");
        let mut b = Bookmarks::load(&dir.join("b.json"));
        b.add("https://a.com", "A", "");
        b.add("https://w1.com", "W1", "工作");
        b.add("https://b.com", "B", "");
        b.add("https://p.com", "P", "工作/專案");
        b.add("https://w2.com", "W2", "工作");
        b.add("https://x.com", "X", "工作區"); // a sibling whose name starts like 工作
        use FolderEntry::*;
        assert_eq!(
            b.folder_entries(""),
            vec![Bookmark(0), Folder("工作".into(), "工作".into()), Bookmark(2), Folder("工作區".into(), "工作區".into())]
        );
        assert_eq!(b.folder_entries("工作"), vec![Bookmark(1), Folder("工作/專案".into(), "專案".into()), Bookmark(4)]);

        // Re-importing files a loose bar bookmark into its source folder, and leaves filed ones alone.
        b.file_if_loose("https://a.com", "其他書籤");
        b.file_if_loose("https://w1.com", "其他書籤");
        assert_eq!(b.items()[0].folder, "其他書籤");
        assert_eq!(b.items()[1].folder, "工作");
    }

    #[test]
    fn history_dedupes_and_orders() {
        let dir = temp_dir("hist");
        let mut h = History::load(&dir.join("h.json"));
        h.record("https://a.com", "A", 1);
        h.record("https://b.com", "B", 2);
        h.record("https://a.com", "", 3);
        h.record("about:blank", "", 4);
        assert_eq!(h.items().len(), 2);
        assert_eq!(h.items()[0].url, "https://a.com");
        assert_eq!(h.items()[0].visits, 2);
        assert_eq!(h.items()[0].title, "A");
        h.save_if_dirty();
        let h2 = History::load(&dir.join("h.json"));
        assert_eq!(h2.items(), h.items());
    }

    #[test]
    fn history_search_matches_all_words() {
        let dir = temp_dir("search");
        let mut h = History::load(&dir.join("h.json"));
        h.record("https://github.com/slint-ui/slint", "Slint GitHub", 1);
        h.record("https://gitlab.com/x", "GitLab", 2);
        assert_eq!(h.search("git slint", 10).len(), 1);
        assert_eq!(h.search("GIT", 10).len(), 2);
    }

    #[test]
    fn suggestions_put_action_first() {
        let dir = temp_dir("sugg");
        let mut bm = Bookmarks::load(&dir.join("b.json"));
        bm.toggle("https://app.clickup.com", "ClickUp");
        let mut h = History::load(&dir.join("h.json"));
        h.record("https://app.clickup.com", "ClickUp", 1);
        h.record("https://clickup.com/blog", "Blog", 2);
        let s = suggestions("clickup", &bm, &h, "https://s/?q={}", 6);
        assert_eq!(s[0].kind, SuggestionKind::Search);
        assert_eq!(s[1].url, "https://app.clickup.com");
        assert_eq!(s.len(), 3, "bookmark and history duplicates are merged");
        let s = suggestions("github.com", &bm, &h, "https://s/?q={}", 6);
        assert_eq!(s[0].kind, SuggestionKind::Url);
        assert_eq!(s[0].url, "https://github.com");
    }

    #[test]
    fn session_roundtrip() {
        let dir = temp_dir("session");
        let s = Session {
            tabs: vec![SessionTab { url: "https://a.com".into(), title: "A".into(), pinned: true }],
            active: 0,
            window: Some(WindowState { width: 800.0, height: 600.0, maximized: true }),
        };
        s.save(&dir.join("s.json"));
        assert_eq!(Session::load(&dir.join("s.json")), s);
        assert_eq!(Session::load(&dir.join("missing.json")), Session::default());
    }

    #[test]
    fn relative_times() {
        assert_eq!(relative_time(100, 130), "剛剛");
        assert_eq!(relative_time(0, 7200), "2 小時前");
    }
}
