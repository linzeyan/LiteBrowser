//! Checking GitHub Releases for a newer build, and swapping the executable at the next start.
//!
//! Windows will not let a running program be overwritten, but it does allow it to be *renamed*.
//! So a download is staged next to the data directory, and at the next start — before any window
//! exists — the running exe is renamed aside and the staged one takes its place.

use std::path::{Path, PathBuf};

/// Where releases are published.
pub const RELEASES_API: &str = "https://api.github.com/repos/linzeyan/LiteBrowser/releases/latest";

/// A release newer than what is running.
#[derive(Clone, Debug, PartialEq)]
pub struct Available {
    pub version: String,
    /// Direct download URL of the bare `LiteBrowser.exe` asset.
    pub exe_url: String,
}

/// Splits "v1.2.3" into numbers, ignoring anything that is not a number.
fn version_parts(version: &str) -> Vec<u32> {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .map(|part| part.chars().take_while(char::is_ascii_digit).collect::<String>())
        .take_while(|digits| !digits.is_empty())
        .filter_map(|digits| digits.parse().ok())
        .collect()
}

/// True when `candidate` is a strictly newer version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (a, b) = (version_parts(candidate), version_parts(current));
    if a.is_empty() {
        return false;
    }
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Reads a GitHub "latest release" response, returning it only when it is newer than `current`
/// and carries a plain `.exe` asset (the zip is for people, the exe is for the updater).
pub fn parse_release(json: &str, current: &str) -> Option<Available> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    if value.get("draft").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        return None;
    }
    let version = value.get("tag_name").and_then(serde_json::Value::as_str)?.to_string();
    if !is_newer(&version, current) {
        return None;
    }
    let exe_url = value
        .get("assets")?
        .as_array()?
        .iter()
        .find(|asset| {
            asset
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case("LiteBrowser.exe"))
        })?
        .get("browser_download_url")?
        .as_str()?
        .to_string();
    Some(Available { version, exe_url })
}

/// Where a downloaded build waits for the next start.
pub fn staged_exe(data_dir: &Path) -> PathBuf {
    data_dir.join("update").join("LiteBrowser.exe")
}

/// The name the outgoing executable is renamed to.
fn retired_exe(exe: &Path) -> PathBuf {
    exe.with_extension("old")
}

/// Result of trying to install a staged update at startup.
#[derive(Debug, PartialEq)]
pub enum Applied {
    /// Nothing was waiting.
    Nothing,
    /// The executable was replaced; the caller should relaunch and exit.
    Replaced,
    Failed(String),
}

/// Installs a staged update over `exe`. Call this before creating any window.
pub fn apply_staged(data_dir: &Path, exe: &Path) -> Applied {
    // A leftover from a previous update: it is no longer running, so it can go now.
    let _ = std::fs::remove_file(retired_exe(exe));

    let staged = staged_exe(data_dir);
    if !staged.is_file() {
        return Applied::Nothing;
    }
    // Refuse to install something that is obviously not a program.
    match std::fs::metadata(&staged) {
        Ok(meta) if meta.len() > 1024 => {}
        _ => {
            let _ = std::fs::remove_file(&staged);
            return Applied::Failed("下載的檔案不完整，已丟棄".into());
        }
    }

    let retired = retired_exe(exe);
    if let Err(e) = std::fs::rename(exe, &retired) {
        return Applied::Failed(format!("無法移開舊版：{e}"));
    }
    match std::fs::rename(&staged, exe).or_else(|_| std::fs::copy(&staged, exe).map(|_| ())) {
        Ok(()) => {
            let _ = std::fs::remove_file(&staged);
            Applied::Replaced
        }
        Err(e) => {
            // Put the old one back so the browser still starts.
            let _ = std::fs::rename(&retired, exe);
            Applied::Failed(format!("無法安裝新版：{e}"))
        }
    }
}

/// Downloads `release` into the staging slot. Windows-only: it needs WinHTTP.
#[cfg(windows)]
pub fn download(release: &Available, data_dir: &Path) -> Result<(), String> {
    // 200 MB is far more than the real binary and still bounded.
    let bytes = crate::net::get(&release.exe_url, "application/octet-stream", 200 * 1024 * 1024)?;
    if bytes.len() < 1024 || &bytes[..2] != b"MZ" {
        return Err("下載的檔案不是 Windows 執行檔".into());
    }
    let staged = staged_exe(data_dir);
    if let Some(parent) = staged.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("無法建立更新資料夾：{e}"))?;
    }
    crate::paths::write_atomic(&staged, &bytes).map_err(|e| format!("無法寫入更新檔：{e}"))
}

/// Asks GitHub whether there is a newer release. Windows-only: it needs WinHTTP.
#[cfg(windows)]
pub fn check(current: &str) -> Result<Option<Available>, String> {
    let body = crate::net::get(RELEASES_API, "application/vnd.github+json", 4 * 1024 * 1024)?;
    let text = String::from_utf8_lossy(&body);
    Ok(parse_release(&text, current))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("v0.2", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        // Garbage never counts as an upgrade.
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("latest", "0.1.0"));
    }

    const RELEASE: &str = r#"{
        "tag_name": "v0.3.0",
        "draft": false,
        "assets": [
            {"name": "LiteBrowser-windows-x64.zip", "browser_download_url": "https://x/zip"},
            {"name": "LiteBrowser.exe", "browser_download_url": "https://x/exe"}
        ]
    }"#;

    #[test]
    fn picks_the_exe_asset_of_a_newer_release() {
        let found = parse_release(RELEASE, "0.1.0").unwrap();
        assert_eq!(found, Available { version: "v0.3.0".into(), exe_url: "https://x/exe".into() });
    }

    #[test]
    fn ignores_releases_that_are_not_upgrades() {
        assert!(parse_release(RELEASE, "0.3.0").is_none());
        assert!(parse_release(RELEASE, "1.0.0").is_none());
    }

    #[test]
    fn ignores_drafts_and_releases_without_an_exe() {
        let draft = RELEASE.replace("\"draft\": false", "\"draft\": true");
        assert!(parse_release(&draft, "0.1.0").is_none());
        let zip_only = r#"{"tag_name":"v9.0.0","assets":[{"name":"x.zip","browser_download_url":"https://x/zip"}]}"#;
        assert!(parse_release(zip_only, "0.1.0").is_none());
        assert!(parse_release("not json", "0.1.0").is_none());
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lb-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn nothing_staged_is_not_an_error() {
        let dir = scratch("none");
        let exe = dir.join("LiteBrowser.exe");
        std::fs::write(&exe, b"MZ current").unwrap();
        assert_eq!(apply_staged(&dir, &exe), Applied::Nothing);
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ current");
    }

    #[test]
    fn a_staged_build_replaces_the_running_one() {
        let dir = scratch("apply");
        let exe = dir.join("LiteBrowser.exe");
        std::fs::write(&exe, b"MZ old").unwrap();
        let staged = staged_exe(&dir);
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, vec![b'M'; 2048]).unwrap();

        assert_eq!(apply_staged(&dir, &exe), Applied::Replaced);
        assert_eq!(std::fs::read(&exe).unwrap().len(), 2048);
        assert!(!staged.exists(), "the staging slot is cleared");

        // The retired copy is removed on the following start.
        assert_eq!(apply_staged(&dir, &exe), Applied::Nothing);
        assert!(!retired_exe(&exe).exists());
    }

    #[test]
    fn a_truncated_download_is_discarded_and_the_old_build_kept() {
        let dir = scratch("short");
        let exe = dir.join("LiteBrowser.exe");
        std::fs::write(&exe, b"MZ good").unwrap();
        let staged = staged_exe(&dir);
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"tiny").unwrap();

        assert!(matches!(apply_staged(&dir, &exe), Applied::Failed(_)));
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ good");
        assert!(!staged.exists());
    }
}
