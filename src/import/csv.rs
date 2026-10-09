//! Parses a password CSV exported from Chrome, Edge or Firefox.
//!
//! All three export a header row plus `url,username,password` columns (with extra columns that
//! differ per browser). Columns are matched by header name, so column order does not matter.

use std::path::Path;

use super::ImportedLogin;

pub fn read_logins_file(path: &Path) -> Result<Vec<ImportedLogin>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("無法讀取 {}：{e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}：{e}", path.display()))
}

pub fn parse(text: &str) -> Result<Vec<ImportedLogin>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // UTF-8 BOM
    let mut rows = parse_rows(text).into_iter();
    let header = rows.next().ok_or("CSV 檔是空的")?;
    let lower: Vec<String> = header.iter().map(|h| h.trim().to_ascii_lowercase()).collect();
    let find = |names: &[&str]| lower.iter().position(|h| names.contains(&h.as_str()));

    let url_i = find(&["url", "login_uri", "origin", "website", "hostname"]).ok_or("找不到 url 欄位")?;
    let user_i = find(&["username", "login_username", "user", "login"]).ok_or("找不到 username 欄位")?;
    let pass_i = find(&["password", "login_password", "pass"]).ok_or("找不到 password 欄位")?;

    let mut out = Vec::new();
    for row in rows {
        let get = |i: usize| row.get(i).map(String::as_str).unwrap_or("").to_string();
        let origin = get(url_i);
        let password = get(pass_i);
        if origin.trim().is_empty() || password.is_empty() {
            continue;
        }
        out.push(ImportedLogin { origin: origin.trim().to_string(), username: get(user_i), password });
    }
    Ok(out)
}

/// Minimal RFC 4180 CSV: quoted fields may contain commas, newlines and `""` escapes.
fn parse_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => row.push(std::mem::take(&mut field)),
            '\r' if !in_quotes => {}
            '\n' if !in_quotes => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|r| !(r.len() == 1 && r[0].trim().is_empty()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_format() {
        let csv = "name,url,username,password,note\n\
                   GitHub,https://github.com,alice,p@ss\\,word,\n\
                   ClickUp,https://app.clickup.com,bob,\"has,comma\",hi\n";
        let logins = parse(csv).unwrap();
        assert_eq!(logins.len(), 2);
        assert_eq!(logins[0].username, "alice");
        assert_eq!(logins[1].password, "has,comma");
        assert_eq!(logins[1].realm(), "https://app.clickup.com/");
    }

    #[test]
    fn firefox_format_and_bom() {
        let csv = "\u{feff}\"url\",\"username\",\"password\",\"httpRealm\",\"formActionOrigin\"\n\
                   \"https://x.com\",\"u\",\"pw\",\"\",\"https://x.com\"\n";
        let logins = parse(csv).unwrap();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].origin, "https://x.com");
    }

    #[test]
    fn firefox_157_export_as_written() {
        // Byte for byte what Firefox 157 exported on the test VM: CRLF, every field quoted except
        // an empty httpRealm, and no line break after the last row.
        let csv = "\"url\",\"username\",\"password\",\"httpRealm\",\"formActionOrigin\",\"guid\",\"timeCreated\",\
                   \"timeLastUsed\",\"timePasswordChanged\"\r\n\
                   \"https://app.clickup.com\",\"firefox-user\",\"f-pw,3\"\"z\",,\"\",\
                   \"{d98fac0e-f236-46b1-94d4-72ccfee543c4}\",\"1791503149152\",\"1791503149152\",\"1791503149152\"";
        let logins = parse(csv).unwrap();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].origin, "https://app.clickup.com");
        assert_eq!(logins[0].username, "firefox-user");
        assert_eq!(logins[0].password, "f-pw,3\"z");
    }

    #[test]
    fn chromium_exports_as_written() {
        // Byte for byte what Chrome 155 and Edge 154 exported on the test VM (Brave matches Edge):
        // CRLF, quotes only where needed, a CRLF inside the quoted note, an empty trailing note.
        let chrome = "name,url,username,password,note\r\n\
                      github.com,https://github.com/login,chrome-user,\"c-pw,1\"\"x\",\"line one\r\n\
                      line two, with comma\"\r\n";
        let edge = "name,url,username,password,note\r\n\
                    stackoverflow.com,https://stackoverflow.com/users/login,edge-user,\"e-pw,4\"\"w\",\r\n";
        for (csv, user, password) in [(chrome, "chrome-user", "c-pw,1\"x"), (edge, "edge-user", "e-pw,4\"w")] {
            let logins = parse(csv).unwrap();
            assert_eq!(logins.len(), 1);
            assert_eq!(logins[0].username, user);
            assert_eq!(logins[0].password, password);
        }
    }

    #[test]
    fn skips_blank_and_passwordless_rows() {
        let csv = "url,username,password\nhttps://a.com,u,pw\n\n,,,\nhttps://b.com,u,\n";
        assert_eq!(parse(csv).unwrap().len(), 1);
    }

    #[test]
    fn missing_column_is_an_error() {
        assert!(parse("url,username\nhttps://a,u\n").is_err());
    }

    #[test]
    fn quoted_newline_in_field() {
        let csv = "url,username,password\n\"https://a.com\",\"li\nne\",pw\n";
        let logins = parse(csv).unwrap();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].username, "li\nne");
    }
}
