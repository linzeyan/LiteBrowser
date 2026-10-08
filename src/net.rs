//! A small HTTPS GET built on WinHTTP.
//!
//! Used for the update check and for fetching an extension from the Chrome Web Store. WinHTTP is
//! part of Windows, so this costs no extra dependency and picks up the machine's proxy settings —
//! which matters on a locked-down VDI.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};

/// Closes a WinHTTP handle when it goes out of scope, including on the error paths.
struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

struct Url<'a> {
    host: &'a str,
    port: u16,
    path: String,
    secure: bool,
}

fn parse_url(url: &str) -> Result<Url<'_>, String> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("網址格式不對：{url}"))?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        other => return Err(format!("不支援的協定：{other}")),
    };
    let split = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(split);
    let path = if path.is_empty() { "/".to_string() } else { path.to_string() };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h, p.parse().unwrap_or(if secure { 443 } else { 80 }))
        }
        _ => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("網址沒有主機名稱：{url}"));
    }
    Ok(Url { host, port, path, secure })
}

/// Fetches a URL, following redirects (WinHTTP does that by default).
/// `max_bytes` caps the download so a bad URL cannot fill the disk.
pub fn get(url: &str, accept: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let parsed = parse_url(url)?;
    unsafe {
        let session = Handle(WinHttpOpen(
            &HSTRING::from("LiteBrowser"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("無法建立 WinHTTP 工作階段".into());
        }
        let connect = Handle(WinHttpConnect(session.0, &HSTRING::from(parsed.host), parsed.port, 0));
        if connect.0.is_null() {
            return Err(format!("無法連線到 {}", parsed.host));
        }
        let flags = if parsed.secure { WINHTTP_FLAG_SECURE } else { Default::default() };
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            &HSTRING::from("GET"),
            &HSTRING::from(parsed.path.as_str()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        ));
        if request.0.is_null() {
            return Err("無法建立 HTTP 要求".into());
        }

        // GitHub's API needs a User-Agent and an explicit Accept.
        let headers: Vec<u16> =
            format!("User-Agent: LiteBrowser\r\nAccept: {accept}\r\n").encode_utf16().collect();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0).map_err(|e| format!("送出要求失敗：{e}"))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| format!("沒有收到回應：{e}"))?;

        let mut status: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut _),
            &mut size,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("讀取狀態碼失敗：{e}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("伺服器回應 HTTP {status}"));
        }

        let mut body = Vec::new();
        loop {
            let mut available: u32 = 0;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(|e| format!("讀取失敗：{e}"))?;
            if available == 0 {
                break;
            }
            let want = (available as usize).min(64 * 1024);
            let start = body.len();
            body.resize(start + want, 0);
            let mut read: u32 = 0;
            WinHttpReadData(request.0, body[start..].as_mut_ptr() as *mut _, want as u32, &mut read)
                .map_err(|e| format!("讀取失敗：{e}"))?;
            body.truncate(start + read as usize);
            if read == 0 {
                break;
            }
            if body.len() > max_bytes {
                return Err(format!("內容超過 {max_bytes} 位元組上限"));
            }
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = parse_url("https://api.github.com/repos/a/b/releases/latest").unwrap();
        assert_eq!(u.host, "api.github.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/repos/a/b/releases/latest");
        assert!(u.secure);

        let u = parse_url("http://example.com:8080").unwrap();
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/");
        assert!(!u.secure);

        assert!(parse_url("ftp://example.com").is_err());
        assert!(parse_url("not a url").is_err());
        assert!(parse_url("https:///path").is_err());
    }
}
