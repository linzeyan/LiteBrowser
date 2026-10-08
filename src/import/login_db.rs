//! Writes imported logins into LiteBrowser's own WebView2 "Login Data" SQLite file.
//!
//! There is no WebView2 API to add a saved password, so the only way to make imported passwords
//! autofill is to insert them into WebView2's own `logins` table, with `password_value` encrypted
//! using WebView2's own key (see `crypto`). This runs at startup, before the WebView2 environment
//! is created, because WebView2 keeps the file locked while it runs.
//!
//! It never touches another browser's store; the logins come from a user-exported CSV.

use std::collections::BTreeSet;
use std::path::Path;


use super::crypto::{self, Key};
use super::{origin_of, ImportedLogin};

#[derive(Debug, Default)]
pub struct Applied {
    pub added: usize,
    pub skipped_existing: usize,
}

/// Inserts `logins` into an existing WebView2 `Login Data` file. The file must already exist
/// (WebView2 creates it on its first run); callers defer the import otherwise.
pub fn apply(login_data: &Path, key: &Key, logins: &[ImportedLogin]) -> Result<Applied, String> {
    if !login_data.is_file() {
        return Err("WebView2 的密碼資料庫尚未建立，請先正常使用一次瀏覽器再重新啟動以完成匯入。".into());
    }
    let conn = super::open_db(login_data)?;
    let columns = table_columns(&conn, "logins")?;
    if !columns.contains("password_value") || !columns.contains("signon_realm") {
        return Err("WebView2 密碼資料庫格式不符，略過密碼匯入。".into());
    }
    let existing = existing_keys(&conn)?;

    let mut applied = Applied::default();
    for login in logins {
        let realm = login.realm();
        if realm.is_empty() {
            continue;
        }
        if existing.contains(&(realm.clone(), login.username.clone())) {
            applied.skipped_existing += 1;
            continue;
        }
        let Some(encrypted) = crypto::encrypt_v10(key, login.password.as_bytes()) else {
            continue;
        };
        let origin = origin_of(&login.origin).map(|o| format!("{o}/")).unwrap_or_else(|| realm.clone());
        insert_login(&conn, &columns, &origin, &realm, &login.username, &encrypted)?;
        applied.added += 1;
    }
    Ok(applied)
}

fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<BTreeSet<String>, String> {
    let mut stmt =
        conn.prepare(&format!("PRAGMA table_info({table})")).map_err(|e| format!("讀取資料庫結構失敗：{e}"))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("讀取資料庫結構失敗：{e}"))?
        .filter_map(Result::ok)
        .collect::<BTreeSet<_>>();
    if cols.is_empty() {
        return Err("找不到 logins 資料表".into());
    }
    Ok(cols)
}

fn existing_keys(conn: &rusqlite::Connection) -> Result<BTreeSet<(String, String)>, String> {
    let mut stmt = conn
        .prepare("SELECT signon_realm, username_value FROM logins")
        .map_err(|e| format!("讀取現有密碼失敗：{e}"))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?.unwrap_or_default())))
        .map_err(|e| format!("讀取現有密碼失敗：{e}"))?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Inserts one row, filling only the columns the table actually has; the rest use their defaults.
fn insert_login(
    conn: &rusqlite::Connection,
    columns: &BTreeSet<String>,
    origin_url: &str,
    signon_realm: &str,
    username: &str,
    password_value: &[u8],
) -> Result<(), String> {
    // Candidate column => value, intersected with the real schema. Chromium requires the first
    // few; the date/int columns are set to sane values so NOT NULL columns are satisfied.
    let now = chromium_now();
    let text_cols: [(&str, &str); 6] = [
        ("origin_url", origin_url),
        ("action_url", ""),
        ("username_element", ""),
        ("username_value", username),
        ("password_element", ""),
        ("signon_realm", signon_realm),
    ];
    let int_cols: [(&str, i64); 9] = [
        ("date_created", now),
        ("date_last_used", 0),
        ("date_password_modified", now),
        ("preferred", 0),
        ("blacklisted_by_user", 0),
        ("scheme", 0),
        ("password_type", 0),
        ("times_used", 0),
        ("skip_zero_click", 0),
    ];

    let mut names: Vec<&str> = Vec::new();
    let mut text_vals: Vec<&str> = Vec::new();
    let mut int_vals: Vec<i64> = Vec::new();
    for (name, value) in text_cols {
        if columns.contains(name) {
            names.push(name);
            text_vals.push(value);
        }
    }
    for (name, value) in int_cols {
        if columns.contains(name) {
            names.push(name);
            int_vals.push(value);
        }
    }
    names.push("password_value");

    let placeholders = (1..=names.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ");
    let sql = format!("INSERT OR IGNORE INTO logins ({}) VALUES ({placeholders})", names.join(", "));

    let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(names.len());
    for v in &text_vals {
        values.push(v);
    }
    for v in &int_vals {
        values.push(v);
    }
    values.push(&password_value);

    conn.execute(&sql, rusqlite::params_from_iter(values)).map_err(|e| format!("寫入密碼失敗：{e}"))?;
    Ok(())
}

/// Microseconds since 1601-01-01 UTC (Chromium's time base).
fn chromium_now() -> i64 {
    const WINDOWS_TO_UNIX_SECS: i64 = 11_644_473_600;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (secs + WINDOWS_TO_UNIX_SECS) * 1_000_000
}

/// Creates a `logins` table close enough to Chromium's for the writer to be exercised in tests.
#[cfg(test)]
fn create_test_logins_table(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "CREATE TABLE logins(
            origin_url TEXT NOT NULL,
            action_url TEXT,
            username_element TEXT,
            username_value TEXT,
            password_element TEXT,
            password_value BLOB,
            signon_realm TEXT NOT NULL,
            date_created INTEGER NOT NULL,
            blacklisted_by_user INTEGER NOT NULL,
            scheme INTEGER NOT NULL,
            password_type INTEGER,
            times_used INTEGER,
            date_last_used INTEGER,
            date_password_modified INTEGER,
            preferred INTEGER,
            skip_zero_click INTEGER,
            UNIQUE(origin_url, username_element, username_value, password_element, signon_realm, scheme));",
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(origin: &str, user: &str, pass: &str) -> ImportedLogin {
        ImportedLogin { origin: origin.into(), username: user.into(), password: pass.into() }
    }

    #[test]
    fn inserts_encrypted_and_dedupes() {
        let dir = super::super::test_dir("logindb");
        let path = dir.join("Login Data");
        let conn = rusqlite::Connection::open(&path).unwrap();
        create_test_logins_table(&conn);
        drop(conn);

        let key: Key = [7u8; 32];
        let logins = vec![
            login("https://github.com/login", "alice", "p@ss"),
            login("https://app.clickup.com/", "bob", "secret"),
        ];
        let applied = apply(&path, &key, &logins).unwrap();
        assert_eq!(applied.added, 2);

        // Second run with an overlapping entry adds only the new one.
        let more = vec![login("https://github.com/login", "alice", "p@ss"), login("https://x.com", "c", "d")];
        let applied = apply(&path, &key, &more).unwrap();
        assert_eq!(applied.added, 1);
        assert_eq!(applied.skipped_existing, 1);

        // The stored password is WebView2-format and decrypts back with the same key.
        let conn = rusqlite::Connection::open(&path).unwrap();
        let blob: Vec<u8> = conn
            .query_row("SELECT password_value FROM logins WHERE username_value='alice'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(crypto::decrypt_v10(&key, &blob).as_deref(), Some(b"p@ss".as_slice()));
        let realm: String = conn
            .query_row("SELECT signon_realm FROM logins WHERE username_value='alice'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(realm, "https://github.com/");
    }

    #[test]
    fn missing_file_defers() {
        let dir = super::super::test_dir("logindb-missing");
        assert!(apply(&dir.join("nope"), &[0u8; 32], &[]).is_err());
    }
}
