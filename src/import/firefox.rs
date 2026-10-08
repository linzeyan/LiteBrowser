//! Firefox: bookmarks and history from `places.sqlite` (unencrypted).

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
    data
}

fn read_bookmarks(conn: &rusqlite::Connection) -> Result<Vec<ImportedBookmark>, String> {
    // moz_bookmarks.type 1 = bookmark; join to its URL and its parent folder's title.
    let mut stmt = conn
        .prepare(
            "SELECT b.title, p.url, parent.title \
             FROM moz_bookmarks b \
             JOIN moz_places p ON b.fk = p.id \
             LEFT JOIN moz_bookmarks parent ON b.parent = parent.id \
             WHERE b.type = 1 AND p.url LIKE 'http%' ORDER BY b.position",
        )
        .map_err(|e| format!("讀取書籤失敗：{e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let title: Option<String> = row.get(0)?;
            let url: String = row.get(1)?;
            let parent: Option<String> = row.get(2)?;
            let folder = match parent.as_deref() {
                // Firefox's built-in roots have no title or a toolbar/menu name.
                None | Some("") | Some("toolbar") => String::new(),
                Some(name) => name.to_string(),
            };
            let title = title.filter(|t| !t.is_empty()).unwrap_or_else(|| url.clone());
            Ok(ImportedBookmark { title, url, folder })
        })
        .map_err(|e| format!("讀取書籤失敗：{e}"))?;
    Ok(rows.filter_map(Result::ok).collect())
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
             CREATE TABLE moz_bookmarks(id INTEGER PRIMARY KEY, type INT, fk INT, parent INT, position INT, title TEXT);
             INSERT INTO moz_places VALUES(1,'https://a.com','A',3,1700000000000000);
             INSERT INTO moz_places VALUES(2,'https://b.com','B',0,NULL);
             INSERT INTO moz_places VALUES(10,'https://clickup.com','ClickUp',1,1700000001000000);
             INSERT INTO moz_bookmarks VALUES(100,2,NULL,0,0,'工作');
             INSERT INTO moz_bookmarks VALUES(101,1,10,100,0,'ClickUp');
             INSERT INTO moz_bookmarks VALUES(102,1,1,0,1,'A');",
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

        let clickup = data.bookmarks.iter().find(|b| b.url == "https://clickup.com").unwrap();
        assert_eq!(clickup.folder, "工作");

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
