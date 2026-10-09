//! Chrome / Edge / Brave: bookmarks (`Bookmarks` JSON) and history (`History` SQLite).
//! Both are stored unencrypted.

use std::path::Path;

use super::{copy_db, open_db, BrowserKind, ImportData, ImportedBookmark, ImportedVisit, Profile, Selection};

/// Lists profiles under a Chromium `User Data` directory, named from `Local State`.
pub fn profiles(kind: BrowserKind, user_data: &Path) -> Vec<Profile> {
    if !user_data.is_dir() {
        return Vec::new();
    }
    let names = profile_names(user_data);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(user_data) else { return out };
    let mut dirs: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && is_profile_dir(p))
        .collect();
    dirs.sort();
    // "Default" first, then "Profile 1", "Profile 2", … in order.
    dirs.sort_by_key(|p| p.file_name().map(|n| n != "Default").unwrap_or(true));
    for dir in dirs {
        let folder = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let name = names.get(&folder).cloned().unwrap_or(folder);
        out.push(Profile { kind, name, dir });
    }
    out
}

fn is_profile_dir(dir: &Path) -> bool {
    dir.join("Bookmarks").is_file() || dir.join("History").is_file() || dir.join("Preferences").is_file()
}

/// Maps folder name → display name from `Local State`'s `profile.info_cache`.
fn profile_names(user_data: &Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(text) = std::fs::read_to_string(user_data.join("Local State")) else { return map };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return map };
    if let Some(cache) = json.pointer("/profile/info_cache").and_then(|v| v.as_object()) {
        for (folder, info) in cache {
            if let Some(name) = info.get("name").and_then(|v| v.as_str()) {
                map.insert(folder.clone(), name.to_string());
            }
        }
    }
    map
}

pub fn read(profile: &Profile, what: Selection, scratch: &Path) -> ImportData {
    let mut data = ImportData::default();
    if what.bookmarks {
        match read_bookmarks(&profile.dir.join("Bookmarks")) {
            Ok(mut b) => data.bookmarks.append(&mut b),
            Err(e) => data.errors.push(e),
        }
    }
    if what.history {
        match read_history(&profile.dir.join("History"), scratch) {
            Ok(mut h) => data.history.append(&mut h),
            Err(e) => data.errors.push(e),
        }
    }
    if what.bookmarks || what.history {
        data.icons = super::read_icons(
            &profile.dir.join("Favicons"),
            scratch,
            "chromium-favicons.sqlite",
            "SELECT m.page_url, b.image_data FROM icon_mapping m JOIN favicon_bitmaps b ON b.icon_id = m.icon_id \
             WHERE b.image_data IS NOT NULL ORDER BY abs(b.width - 32)",
        );
    }
    data
}

pub fn read_bookmarks(path: &Path) -> Result<Vec<ImportedBookmark>, String> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("無法讀取書籤：{e}"))?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("書籤格式錯誤：{e}"))?;
    let mut out = Vec::new();
    if let Some(roots) = json.get("roots").and_then(|v| v.as_object()) {
        // "書籤列" keeps items at the top level; other roots become named folders.
        for (root_key, node) in roots {
            let top = match root_key.as_str() {
                "bookmark_bar" => String::new(),
                "other" => "其他書籤".to_string(),
                "synced" => "行動裝置書籤".to_string(),
                _ => continue,
            };
            // The root node itself is a folder ("Bookmarks bar"); its name is not part of the path,
            // so walk its children directly under `top`.
            if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
                for child in children {
                    walk_bookmarks(child, &top, &mut out);
                }
            }
        }
    }
    Ok(out)
}

fn walk_bookmarks(node: &serde_json::Value, folder: &str, out: &mut Vec<ImportedBookmark>) {
    match node.get("type").and_then(|v| v.as_str()) {
        Some("url") => {
            if let Some(url) = node.get("url").and_then(|v| v.as_str()) {
                let title = node.get("name").and_then(|v| v.as_str()).unwrap_or(url);
                out.push(ImportedBookmark { title: title.to_string(), url: url.to_string(), folder: folder.to_string() });
            }
        }
        Some("folder") => {
            // "/" separates path segments, so a slash inside a name becomes a full-width one.
            let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("").replace('/', "／");
            let sub = if folder.is_empty() { name } else { format!("{folder}/{name}") };
            if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
                for child in children {
                    walk_bookmarks(child, &sub, out);
                }
            }
        }
        _ => {}
    }
}

fn read_history(path: &Path, scratch: &Path) -> Result<Vec<ImportedVisit>, String> {
    let db = copy_db(path, scratch, "chromium-history.sqlite")?;
    let conn = open_db(&db)?;
    let mut stmt = conn
        .prepare(
            "SELECT url, title, visit_count, last_visit_time FROM urls \
             WHERE hidden = 0 AND url LIKE 'http%' ORDER BY last_visit_time DESC LIMIT 20000",
        )
        .map_err(|e| format!("讀取歷史紀錄失敗：{e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let url: String = row.get(0)?;
            let title: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let visits: i64 = row.get(2)?;
            let ts: i64 = row.get(3)?;
            Ok(ImportedVisit { url, title, visits: visits.max(0) as u32, last_visit: chromium_time(ts) })
        })
        .map_err(|e| format!("讀取歷史紀錄失敗：{e}"))?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Chromium timestamps are microseconds since 1601-01-01 UTC.
fn chromium_time(micros: i64) -> u64 {
    const WINDOWS_TO_UNIX_SECS: i64 = 11_644_473_600;
    (micros / 1_000_000 - WINDOWS_TO_UNIX_SECS).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_bookmarks() {
        let json = r#"{"roots":{
            "bookmark_bar":{"type":"folder","name":"書籤列","children":[
                {"type":"url","name":"GitHub","url":"https://github.com"},
                {"type":"folder","name":"工作","children":[
                    {"type":"url","name":"ClickUp","url":"https://app.clickup.com"}
                ]}
            ]},
            "other":{"type":"folder","name":"其他","children":[
                {"type":"url","name":"Blog","url":"https://blog.example"}
            ]},
            "synced":{"type":"folder","name":"mobile","children":[]}
        }}"#;
        let dir = super::super::test_dir("bm");
        let path = dir.join("Bookmarks");
        std::fs::write(&path, json).unwrap();
        let mut b = read_bookmarks(&path).unwrap();
        b.sort_by(|a, c| a.url.cmp(&c.url));
        assert_eq!(b.len(), 3);
        let clickup = b.iter().find(|x| x.url == "https://app.clickup.com").unwrap();
        assert_eq!(clickup.folder, "工作");
        let github = b.iter().find(|x| x.url == "https://github.com").unwrap();
        assert_eq!(github.folder, "");
        let blog = b.iter().find(|x| x.url == "https://blog.example").unwrap();
        assert_eq!(blog.folder, "其他書籤");
    }

    #[test]
    fn reads_history_sqlite() {
        let dir = super::super::test_dir("hist");
        let src = dir.join("History");
        let conn = rusqlite::Connection::open(&src).unwrap();
        conn.execute_batch(
            "CREATE TABLE urls(url TEXT, title TEXT, visit_count INT, last_visit_time INT, hidden INT);
             INSERT INTO urls VALUES('https://a.com','A',5,13300000000000000,0);
             INSERT INTO urls VALUES('https://b.com','B',1,13300000001000000,0);
             INSERT INTO urls VALUES('chrome://x','X',1,13300000002000000,0);
             INSERT INTO urls VALUES('https://h.com','H',1,13300000003000000,1);",
        )
        .unwrap();
        drop(conn);
        let visits = read_history(&src, &dir.join("scratch")).unwrap();
        let urls: Vec<&str> = visits.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(urls, vec!["https://b.com", "https://a.com"]);
        assert!(visits[1].last_visit > 1_600_000_000);
    }
}
