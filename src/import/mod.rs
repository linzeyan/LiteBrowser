//! Importing data from Chrome, Edge, Brave and Firefox.
//!
//! Scope, chosen so the code only ever reads non-sensitive data from other browsers:
//!   * **Bookmarks** and **History** are read directly — they are plaintext/unencrypted.
//!   * **Passwords** are imported from a CSV the user exports from the source browser (every major
//!     browser has a built-in, OS-authenticated "Export passwords"). LiteBrowser writes them into
//!     its *own* WebView2 store (see `login_db`). It never reads another browser's password store;
//!     that also avoids Chrome's app-bound encryption, which blocks direct reads anyway.
//!
//! Readers are plain functions over files, so they are unit tested on any platform.

pub mod chromium;
pub mod crypto;
pub mod csv;
pub mod firefox;
pub mod login_db;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Decrypts a Windows DPAPI blob for the current user (used only for LiteBrowser's own key).
pub type Unprotect<'a> = &'a dyn Fn(&[u8]) -> Option<Vec<u8>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserKind {
    Chrome,
    Edge,
    Brave,
    Firefox,
}

impl BrowserKind {
    pub fn label(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Google Chrome",
            BrowserKind::Edge => "Microsoft Edge",
            BrowserKind::Brave => "Brave",
            BrowserKind::Firefox => "Mozilla Firefox",
        }
    }
}

/// One browser profile found on this machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub kind: BrowserKind,
    /// Display name of the profile ("Default", "工作", …).
    pub name: String,
    /// The profile folder (Chromium: `User Data\Default`; Firefox: the profile directory).
    pub dir: PathBuf,
}

impl Profile {
    pub fn label(&self) -> String {
        format!("{}（{}）", self.kind.label(), self.name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportedBookmark {
    pub title: String,
    pub url: String,
    /// "" = top level; otherwise a path like "書籤列/工作".
    pub folder: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportedVisit {
    pub url: String,
    pub title: String,
    pub visits: u32,
    /// Unix seconds.
    pub last_visit: u64,
}

/// A login to add to LiteBrowser's own store. Always comes from a user-exported CSV.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedLogin {
    /// Page the login is for, e.g. "https://github.com/login".
    pub origin: String,
    pub username: String,
    pub password: String,
}

impl ImportedLogin {
    /// Chromium's grouping key, e.g. "https://github.com/".
    pub fn realm(&self) -> String {
        origin_of(&self.origin).map(|o| format!("{o}/")).unwrap_or_default()
    }
}

/// "https://example.com:8443" from "https://example.com:8443/path?x".
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    (!host.is_empty() && !scheme.is_empty()).then(|| format!("{}://{}", scheme.to_ascii_lowercase(), host))
}

/// What was read from one profile.
#[derive(Debug, Default)]
pub struct ImportData {
    pub bookmarks: Vec<ImportedBookmark>,
    pub history: Vec<ImportedVisit>,
    pub errors: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Selection {
    pub bookmarks: bool,
    pub history: bool,
}

/// Reads the selected non-sensitive data (bookmarks, history) from one profile.
pub fn read_profile(profile: &Profile, what: Selection, scratch: &Path) -> ImportData {
    match profile.kind {
        BrowserKind::Firefox => firefox::read(profile, what, scratch),
        _ => chromium::read(profile, what, scratch),
    }
}

/// Finds browser profiles under `%LOCALAPPDATA%` and `%APPDATA%`.
pub fn detect_profiles(local_appdata: &Path, roaming_appdata: &Path) -> Vec<Profile> {
    let mut out = Vec::new();
    for (kind, rel) in [
        (BrowserKind::Edge, "Microsoft/Edge/User Data"),
        (BrowserKind::Chrome, "Google/Chrome/User Data"),
        (BrowserKind::Brave, "BraveSoftware/Brave-Browser/User Data"),
    ] {
        out.extend(chromium::profiles(kind, &local_appdata.join(rel)));
    }
    out.extend(firefox::profiles(&roaming_appdata.join("Mozilla/Firefox")));
    out
}

/// Copies a database to a scratch folder so it can be read while the browser holds a lock on it.
pub(crate) fn copy_db(src: &Path, scratch: &Path, name: &str) -> Result<PathBuf, String> {
    if !src.is_file() {
        return Err(format!("找不到 {}", src.display()));
    }
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let dst = scratch.join(name);
    std::fs::copy(src, &dst).map_err(|e| format!("無法讀取 {}（請先關閉該瀏覽器）：{e}", src.display()))?;
    for suffix in ["-wal", "-journal"] {
        let side = PathBuf::from(format!("{}{suffix}", src.display()));
        let side_dst = PathBuf::from(format!("{}{suffix}", dst.display()));
        let _ = std::fs::remove_file(&side_dst);
        if side.is_file() {
            let _ = std::fs::copy(&side, &side_dst);
        }
    }
    Ok(dst)
}

pub(crate) fn open_db(path: &Path) -> Result<rusqlite::Connection, String> {
    rusqlite::Connection::open(path).map_err(|e| format!("無法開啟 {}：{e}", path.display()))
}

// ---------------------------------------------------------------------------------------------
// Pending password imports (applied at startup, before WebView2 locks its own Login Data)

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingImport {
    /// Paths of user-exported password CSV files waiting to be applied.
    pub password_csvs: Vec<PathBuf>,
}

impl PendingImport {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if self.password_csvs.is_empty() {
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::paths::write_atomic(path, &bytes)
    }

    pub fn add_csv(&mut self, path: PathBuf) {
        if !self.password_csvs.contains(&path) {
            self.password_csvs.push(path);
        }
    }
}

#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("litebrowser-import-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_and_realm() {
        assert_eq!(origin_of("https://GitHub.com/login?x=1").as_deref(), Some("https://GitHub.com"));
        assert_eq!(origin_of("http://localhost:3000").as_deref(), Some("http://localhost:3000"));
        assert_eq!(origin_of("nope"), None);
        let login = ImportedLogin { origin: "https://a.com/login".into(), username: "u".into(), password: "p".into() };
        assert_eq!(login.realm(), "https://a.com/");
    }

    #[test]
    fn pending_roundtrip() {
        let dir = test_dir("pending");
        let path = dir.join("p.json");
        let mut p = PendingImport::default();
        p.add_csv(PathBuf::from("C:/x.csv"));
        p.add_csv(PathBuf::from("C:/x.csv"));
        assert_eq!(p.password_csvs.len(), 1);
        p.save(&path).unwrap();
        assert_eq!(PendingImport::load(&path), p);
        PendingImport::default().save(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn detects_profiles() {
        let dir = test_dir("detect");
        let local = dir.join("local");
        let roaming = dir.join("roaming");
        let chrome = local.join("Google/Chrome/User Data");
        std::fs::create_dir_all(chrome.join("Default")).unwrap();
        std::fs::create_dir_all(chrome.join("Profile 2")).unwrap();
        std::fs::write(chrome.join("Default/Bookmarks"), "{}").unwrap();
        std::fs::write(chrome.join("Profile 2/History"), "").unwrap();
        std::fs::write(
            chrome.join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{"name":"個人"},"Profile 2":{"name":"工作"}}}}"#,
        )
        .unwrap();
        let ff = roaming.join("Mozilla/Firefox");
        std::fs::create_dir_all(ff.join("Profiles/abc.default-release")).unwrap();
        std::fs::write(ff.join("Profiles/abc.default-release/places.sqlite"), "").unwrap();
        std::fs::write(
            ff.join("profiles.ini"),
            "[Profile0]\nName=default-release\nIsRelative=1\nPath=Profiles/abc.default-release\n",
        )
        .unwrap();

        let labels: Vec<String> = detect_profiles(&local, &roaming).iter().map(Profile::label).collect();
        assert_eq!(
            labels,
            vec!["Google Chrome（個人）", "Google Chrome（工作）", "Mozilla Firefox（default-release）"]
        );
    }
}
