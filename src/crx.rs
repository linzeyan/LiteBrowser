//! Installing an extension from the Chrome Web Store.
//!
//! WebView2 can only load an *unpacked* extension folder, so a store install is three steps:
//! work out the extension id from the URL the user pasted, download the `.crx` the store serves,
//! and unpack it into our extensions folder. A `.crx` is a short header followed by an ordinary
//! zip, so the only real work is stripping that header and extracting safely.

use std::io::Read;
use std::path::{Path, PathBuf};

/// A Chrome Web Store extension id: 32 letters a–p.
fn is_extension_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| b.is_ascii_lowercase() && b <= b'p')
}

/// Pulls the extension id out of whatever the user pasted: a full store URL, a `/detail/...`
/// path, or the bare id.
pub fn parse_store_url(input: &str) -> Option<String> {
    let text = input.trim();
    if is_extension_id(text) {
        return Some(text.to_string());
    }
    // Both the old (chrome.google.com/webstore/detail/slug/id) and new
    // (chromewebstore.google.com/detail/slug/id) shapes end with the id, possibly followed by
    // a query or fragment; some locales insert a language segment before `detail`.
    let body = text.split(['?', '#']).next()?;
    body.split('/').rev().map(|part| part.trim()).find(|part| is_extension_id(part)).map(str::to_string)
}

/// The store's own download endpoint — the one Chrome itself asks for an update.
/// `prodversion` has to look like a real Chrome version or the store answers 204.
pub fn download_url(id: &str) -> String {
    format!(
        "https://clients2.google.com/service/update2/crx\
         ?response=redirect&acceptformat=crx2,crx3&prodversion=140.0.0.0&x=id%3D{id}%26uc"
    )
}

/// Strips the CRX wrapper, returning the zip inside.
///
/// CRX3: `Cr24`, version 3, header length, then that many bytes of protobuf, then the zip.
/// CRX2: `Cr24`, version 2, public-key length, signature length, then both, then the zip.
pub fn zip_payload(bytes: &[u8]) -> Result<&[u8], String> {
    if bytes.len() < 16 {
        return Err("下載的檔案太小，不是擴充功能".into());
    }
    if &bytes[..4] == b"PK\x03\x04" {
        // Already a plain zip (some mirrors serve one).
        return Ok(bytes);
    }
    if &bytes[..4] != b"Cr24" {
        return Err("下載的檔案不是 Chrome 擴充功能（.crx）".into());
    }
    let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
    let start = match word(4) {
        3 => 12 + word(8),
        2 => 16 + word(8) + word(12),
        other => return Err(format!("不支援的 .crx 版本：{other}")),
    };
    bytes.get(start..).filter(|rest| rest.len() > 4).ok_or_else(|| "擴充功能檔案不完整".into())
}

/// Rejects entry names that would write outside the destination folder, and the `_metadata`
/// folder the store adds (WebView2 refuses an unpacked extension that still carries it).
fn safe_entry_path(name: &str) -> Option<PathBuf> {
    let name = name.replace('\\', "/");
    if name.ends_with('/') {
        return None;
    }
    let mut path = PathBuf::new();
    for part in name.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            "_metadata" => return None,
            _ => {}
        }
        if part.contains(':') {
            return None;
        }
        path.push(part);
    }
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
}

/// Extracts a zip into `dest`, which is emptied first. Returns how many files were written.
pub fn unpack(zip_bytes: &[u8], dest: &Path) -> Result<usize, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .map_err(|e| format!("無法讀取擴充功能內容：{e}"))?;

    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).map_err(|e| format!("無法建立資料夾：{e}"))?;

    let mut written = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("無法展開檔案：{e}"))?;
        let Some(relative) = safe_entry_path(entry.name()) else { continue };
        let target = dest.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("無法建立資料夾：{e}"))?;
        }
        let mut body = Vec::new();
        entry.read_to_end(&mut body).map_err(|e| format!("無法讀取 {}：{e}", relative.display()))?;
        std::fs::write(&target, &body).map_err(|e| format!("無法寫入 {}：{e}", relative.display()))?;
        written += 1;
    }
    if !dest.join("manifest.json").is_file() {
        return Err("擴充功能內容缺少 manifest.json".into());
    }
    Ok(written)
}

/// The display name from an unpacked extension's manifest, resolving `__MSG_name__` against the
/// bundled locale files. Falls back to the folder name.
pub fn manifest_name(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = manifest.get("name").and_then(serde_json::Value::as_str)?;
    let Some(key) = name.strip_prefix("__MSG_").and_then(|rest| rest.strip_suffix("__")) else {
        return Some(name.to_string());
    };
    let default_locale = manifest.get("default_locale").and_then(serde_json::Value::as_str).unwrap_or("en");
    for locale in [default_locale, "en", "en_US"] {
        let path = dir.join("_locales").join(locale).join("messages.json");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(messages) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let message = messages.get(key).and_then(|m| m.get("message")).and_then(serde_json::Value::as_str);
        if let Some(message) = message {
            return Some(message.to_string());
        }
    }
    None
}

/// Downloads an extension and unpacks it under `extensions_dir`, returning its folder name.
/// Windows-only: it needs WinHTTP.
#[cfg(windows)]
pub fn install(id: &str, extensions_dir: &Path) -> Result<String, String> {
    // 64 MB is generous for an extension and still bounded.
    let bytes = crate::net::get(&download_url(id), "application/octet-stream", 64 * 1024 * 1024)?;
    let zip_bytes = zip_payload(&bytes)?;
    let dest = extensions_dir.join(id);
    unpack(zip_bytes, &dest)?;
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "modkelfkcfjpgbfmnbnllalkiogfofhb";

    #[test]
    fn reads_ids_out_of_store_urls() {
        assert_eq!(
            parse_store_url("https://chromewebstore.google.com/detail/read-frog-translate-learn/modkelfkcfjpgbfmnbnllalkiogfofhb")
                .as_deref(),
            Some(ID)
        );
        assert_eq!(
            parse_store_url(&format!("https://chrome.google.com/webstore/detail/read-frog/{ID}?hl=zh-TW")).as_deref(),
            Some(ID)
        );
        assert_eq!(parse_store_url(&format!("  {ID}  ")).as_deref(), Some(ID));
        assert_eq!(
            parse_store_url(&format!("https://chromewebstore.google.com/zh-TW/detail/slug/{ID}/reviews")).as_deref(),
            Some(ID)
        );

        assert_eq!(parse_store_url("https://example.com/"), None);
        // 32 characters, but outside the a–p alphabet store ids use.
        assert_eq!(parse_store_url("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"), None);
        assert_eq!(parse_store_url(""), None);
    }

    #[test]
    fn download_url_names_the_extension() {
        let url = download_url(ID);
        assert!(url.starts_with("https://clients2.google.com/service/update2/crx?"), "{url}");
        assert!(url.contains(ID));
    }

    fn crx3(payload: &[u8], header: &[u8]) -> Vec<u8> {
        let mut bytes = b"Cr24".to_vec();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&(header.len() as u32).to_le_bytes());
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn strips_the_crx_header() {
        let payload = b"PK\x03\x04 pretend this is a zip";
        assert_eq!(zip_payload(&crx3(payload, &[7; 40])).unwrap(), payload);

        // CRX2: key length and signature length instead of a header block.
        let mut crx2 = b"Cr24".to_vec();
        crx2.extend_from_slice(&2u32.to_le_bytes());
        crx2.extend_from_slice(&3u32.to_le_bytes());
        crx2.extend_from_slice(&5u32.to_le_bytes());
        crx2.extend_from_slice(&[1, 2, 3, 9, 9, 9, 9, 9]);
        crx2.extend_from_slice(payload);
        assert_eq!(zip_payload(&crx2).unwrap(), payload);

        // A bare zip is passed through; anything else is refused.
        assert_eq!(zip_payload(payload).unwrap(), payload);
        assert!(zip_payload(b"<!doctype html><html>not an extension</html>").is_err());
        assert!(zip_payload(b"short").is_err());
        assert!(zip_payload(&crx3(b"", &[0; 4])).is_err(), "no zip after the header");
    }

    #[test]
    fn entry_paths_cannot_escape() {
        assert_eq!(safe_entry_path("manifest.json"), Some(PathBuf::from("manifest.json")));
        assert_eq!(safe_entry_path("js/content.js"), Some(PathBuf::from("js").join("content.js")));
        assert_eq!(safe_entry_path("js\\content.js"), Some(PathBuf::from("js").join("content.js")));
        assert_eq!(safe_entry_path("./a.js"), Some(PathBuf::from("a.js")));

        assert_eq!(safe_entry_path("../evil.js"), None);
        assert_eq!(safe_entry_path("a/../../evil.js"), None);
        assert_eq!(safe_entry_path("C:/evil.js"), None);
        assert_eq!(safe_entry_path("_metadata/verified_contents.json"), None);
        assert_eq!(safe_entry_path("js/"), None);
        assert_eq!(safe_entry_path(""), None);
    }

    /// Builds a zip in memory so the extractor is exercised for real.
    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in files {
            writer.start_file(*name, SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut writer, body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lb-crx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn unpacks_an_extension() {
        let dir = scratch("unpack");
        let archive = zip_of(&[
            ("manifest.json", r#"{"name":"Read Frog","version":"1.0"}"#),
            ("js/content.js", "console.log(1)"),
            ("_metadata/verified_contents.json", "[]"),
            ("../escape.js", "nope"),
        ]);
        assert_eq!(unpack(&archive, &dir).unwrap(), 2, "only the two real files");
        assert!(dir.join("manifest.json").is_file());
        assert_eq!(std::fs::read_to_string(dir.join("js/content.js")).unwrap(), "console.log(1)");
        assert!(!dir.join("_metadata").exists());
        assert!(!dir.parent().unwrap().join("escape.js").exists());
        assert_eq!(manifest_name(&dir).as_deref(), Some("Read Frog"));

        // Installing again replaces the folder rather than merging into it.
        let second = zip_of(&[("manifest.json", r#"{"name":"Read Frog","version":"2.0"}"#)]);
        assert_eq!(unpack(&second, &dir).unwrap(), 1);
        assert!(!dir.join("js").exists(), "the old files are gone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_archive_without_a_manifest_is_refused() {
        let dir = scratch("nomanifest");
        let archive = zip_of(&[("readme.txt", "hello")]);
        assert!(unpack(&archive, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_localised_names() {
        let dir = scratch("locale");
        let archive = zip_of(&[
            ("manifest.json", r#"{"name":"__MSG_extName__","default_locale":"zh_TW"}"#),
            ("_locales/zh_TW/messages.json", r#"{"extName":{"message":"讀蛙"}}"#),
        ]);
        unpack(&archive, &dir).unwrap();
        assert_eq!(manifest_name(&dir).as_deref(), Some("讀蛙"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
