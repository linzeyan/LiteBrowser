//! Firefox: bookmarks and history from `places.sqlite` (unencrypted).

use std::collections::HashMap;
use std::path::Path;

use super::{copy_db, open_db, BrowserKind, ImportData, ImportedBookmark, ImportedVisit, Profile, Selection};

/// Lists Firefox profiles from `profiles.ini`.
pub fn profiles(firefox_dir: &Path) -> Vec<Profile> {
    let ini = firefox_dir.join("profiles.ini");
    let Ok(text) = std::fs::read_to_string(&ini) else { return Vec::new() };
    let mut out = Vec::new();
    for section in text.split('[').filter(|s| s.starts_with("Profile")) {
        let mut name = String::new();
        let mut path = String::new();
        let mut is_relative = true;
        for line in section.lines() {
            if let Some(v) = line.strip_prefix("Name=") {
                name = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("Path=") {
                path = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("IsRelative=") {
                is_relative = v.trim() != "0";
            }
        }
        if path.is_empty() {
            continue;
        }
        let dir = if is_relative { firefox_dir.join(path.replace('/', std::path::MAIN_SEPARATOR_STR)) } else { path.into() };
        if dir.join("places.sqlite").is_file() {
            out.push(Profile { kind: BrowserKind::Firefox, name, dir });
        }
    }
    out
}

pub fn read(profile: &Profile, what: Selection, scratch: &Path) -> ImportData {
    let mut data = ImportData::default();
    if !what.bookmarks && !what.history {
        return data;
    }
    let places = match copy_db(&profile.dir.join("places.sqlite"), scratch, "firefox-places.sqlite") {
        Ok(p) => p,
        Err(e) => {
            data.errors.push(e);
            return data;
        }
    };
    let conn = match open_db(&places) {
        Ok(c) => c,
        Err(e) => {
            data.errors.push(e);
            return data;
        }
    };
    if what.bookmarks {
        match read_bookmarks(&conn) {
            Ok(mut b) => data.bookmarks.append(&mut b),
            Err(e) => data.errors.push(e),
        }
    }
    if what.history {
        match read_history(&conn) {
            Ok(mut h) => data.history.append(&mut h),
            Err(e) => data.errors.push(e),
        }
    }
    // Page icons, plus root icons (/favicon.ico), which Firefox keeps per site, not per page.
    data.icons = super::read_icons(
        &profile.dir.join("favicons.sqlite"),
        scratch,
        "firefox-favicons.sqlite",
        "SELECT url, data FROM (\
             SELECT p.page_url AS url, i.data AS data, i.width AS width FROM moz_icons_to_pages ip \
             JOIN moz_pages_w_icons p ON p.id = ip.page_id JOIN moz_icons i ON i.id = ip.icon_id \
             UNION ALL SELECT icon_url, data, width FROM moz_icons WHERE root = 1) \
         WHERE data IS NOT NULL ORDER BY abs(width - 32)",
    );
    data
}

fn read_bookmarks(conn: &rusqlite::Connection) -> Result<Vec<ImportedBookmark>, String> {
    let err = |e: rusqlite::Error| format!("讀取書籤失敗：{e}");
    // moz_bookmarks.type 2 = folder: id → (parent, title, guid), to spell out full folder paths.
    let mut stmt = conn.prepare("SELECT id, parent, title, guid FROM moz_bookmarks WHERE type = 2").map_err(err)?;
    let folders: HashMap<i64, (i64, String, String)> = stmt
        .query_map([], |row| {
            let title: Option<String> = row.get(2)?;
            Ok((row.get(0)?, (row.get(1)?, title.unwrap_or_default(), row.get(3)?)))
        })
        .map_err(err)?
        .filter_map(Result::ok)
        .collect();
    // type 1 = bookmark.
    let mut stmt = conn
        .prepare(
            "SELECT b.title, p.url, b.parent FROM moz_bookmarks b JOIN moz_places p ON b.fk = p.id \
             WHERE b.type = 1 AND p.url LIKE 'http%' ORDER BY b.position",
        )
        .map_err(err)?;
    let rows = stmt
        .query_map([], |row| {
            let title: Option<String> = row.get(0)?;
            let url: String = row.get(1)?;
            let folder = folder_path(&folders, row.get(2)?);
            let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| url.clone());
            Ok(ImportedBookmark { title, url, folder })
        })
        .map_err(err)?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Walks up to a built-in root: the toolbar is our bookmarks bar, the other roots become folders
/// named the way Chromium import names its own.
fn folder_path(folders: &HashMap<i64, (i64, String, String)>, mut id: i64) -> String {
    let mut names = Vec::new();
    while let Some((parent, title, guid)) = folders.get(&id) {
        match guid.as_str() {
            "toolbar_____" | "root________" => break,
            "menu________" => {
                names.push("書籤選單".to_string());
                break;
            }
            "unfiled_____" => {
                names.push(crate::storage::OTHER_FOLDER.to_string());
                break;
            }
            "mobile______" => {
                names.push("行動裝置書籤".to_string());
                break;
            }
            _ => names.push(title.replace('/', "／")),
        }
        id = *parent;
    }
    names.reverse();
    names.join("/")
}

fn read_history(conn: &rusqlite::Connection) -> Result<Vec<ImportedVisit>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT url, title, visit_count, last_visit_date FROM moz_places \
             WHERE url LIKE 'http%' AND visit_count > 0 ORDER BY last_visit_date DESC LIMIT 20000",
        )
        .map_err(|e| format!("讀取歷史紀錄失敗：{e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let url: String = row.get(0)?;
            let title: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let visits: i64 = row.get(2)?;
            // last_visit_date is microseconds since the Unix epoch; may be NULL.
            let micros: Option<i64> = row.get(3)?;
            Ok(ImportedVisit {
                url,
                title,
                visits: visits.max(0) as u32,
                last_visit: micros.map(|m| (m / 1_000_000).max(0) as u64).unwrap_or(0),
            })
        })
        .map_err(|e| format!("讀取歷史紀錄失敗：{e}"))?;
    Ok(rows.filter_map(Result::ok).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_places(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("places.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE moz_places(id INTEGER PRIMARY KEY, url TEXT, title TEXT, visit_count INT, last_visit_date INT);
             CREATE TABLE moz_bookmarks(id INTEGER PRIMARY KEY, type INT, fk INT, parent INT, position INT, title TEXT, guid TEXT);
             INSERT INTO moz_places VALUES(1,'https://a.com','A',3,1700000000000000);
             INSERT INTO moz_places VALUES(2,'https://b.com','B',0,NULL);
             INSERT INTO moz_places VALUES(10,'https://clickup.com','ClickUp',1,1700000001000000);
             INSERT INTO moz_places VALUES(11,'https://support.mozilla.org/','Get Help',0,NULL);
             INSERT INTO moz_bookmarks VALUES(1,2,NULL,0,0,'','root________');
             INSERT INTO moz_bookmarks VALUES(2,2,NULL,1,0,'menu','menu________');
             INSERT INTO moz_bookmarks VALUES(3,2,NULL,1,1,'toolbar','toolbar_____');
             INSERT INTO moz_bookmarks VALUES(100,2,NULL,3,0,'工作','f-work');
             INSERT INTO moz_bookmarks VALUES(103,2,NULL,2,0,'Mozilla Firefox','f-moz');
             INSERT INTO moz_bookmarks VALUES(101,1,10,100,0,'ClickUp','b-1');
             INSERT INTO moz_bookmarks VALUES(102,1,1,3,1,'A','b-2');
             INSERT INTO moz_bookmarks VALUES(104,1,11,103,0,'Get Help','b-3');",
        )
        .unwrap();
        path
    }

    #[test]
    fn reads_bookmarks_and_history() {
        let dir = super::super::test_dir("ff");
        let src_dir = dir.join("profile");
        std::fs::create_dir_all(&src_dir).unwrap();
        make_places(&src_dir);
        let profile = Profile { kind: BrowserKind::Firefox, name: "p".into(), dir: src_dir };
        let data = read(&profile, Selection { bookmarks: true, history: true }, &dir.join("scratch"));
        assert!(data.errors.is_empty(), "{:?}", data.errors);

        // Full paths: the toolbar is the bar itself, the menu root becomes a named folder.
        let folder = |url: &str| data.bookmarks.iter().find(|b| b.url == url).unwrap().folder.clone();
        assert_eq!(folder("https://clickup.com"), "工作");
        assert_eq!(folder("https://a.com"), "");
        assert_eq!(folder("https://support.mozilla.org/"), "書籤選單/Mozilla Firefox");

        // Only visited pages (visit_count > 0) appear in history, newest first.
        let urls: Vec<&str> = data.history.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(urls, vec!["https://clickup.com", "https://a.com"]);
    }

    #[test]
    fn parses_profiles_ini() {
        let dir = super::super::test_dir("ffprof");
        std::fs::create_dir_all(dir.join("Profiles/x.default")).unwrap();
        std::fs::write(dir.join("Profiles/x.default/places.sqlite"), "").unwrap();
        std::fs::write(
            dir.join("profiles.ini"),
            "[Profile0]\nName=我的\nIsRelative=1\nPath=Profiles/x.default\n\n[Install123]\nDefault=Profiles/x.default\n",
        )
        .unwrap();
        let profiles = profiles(&dir);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "我的");
    }
}
