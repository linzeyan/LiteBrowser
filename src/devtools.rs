//! Docked DevTools.
//!
//! WebView2's own `OpenDevToolsWindow` only opens a separate window. To dock DevTools inside our
//! window we instead run Chromium's remote-debugging server (enabled with a browser switch) and
//! load its DevTools frontend into a second WebView placed beside the page.
//!
//! The port is chosen by Chromium (switch value 0) and written to `DevToolsActivePort` in the
//! user-data folder. For a page we fetch `/json/list` over plain localhost HTTP and point the
//! frontend the runtime serves itself at that target's WebSocket.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// Reads the actual remote-debugging port Chromium chose.
pub fn read_active_port(user_data: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(user_data.join("DevToolsActivePort")).ok()?;
    text.lines().next()?.trim().parse().ok()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub kind: String,
    pub url: String,
    pub ws_debugger_url: String,
}

/// Fetches and parses the list of debuggable targets. On failure the error carries a snippet of
/// what the server actually said, so the log explains why DevTools could not be docked.
pub fn fetch_targets(port: u16) -> Result<Vec<Target>, String> {
    let mut last_err = String::new();
    // Older/newer Chromium builds expose one or the other.
    for path in ["/json/list", "/json"] {
        match http_get(port, path) {
            Ok(body) => match parse_targets(&body) {
                Some(targets) => return Ok(targets),
                None => last_err = format!("{path} 回應不是目標清單：{}", snippet(&body)),
            },
            Err(e) => last_err = format!("{path}：{e}"),
        }
    }
    Err(last_err)
}

fn snippet(body: &str) -> String {
    let text: String = body.trim().chars().take(200).collect();
    text.replace('\n', " ")
}

fn http_get(port: u16, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("連線失敗：{e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    // Chromium's DevTools HTTP server validates the Host header and rejects anything that is not
    // an IP address or "localhost"; it must also carry the port.
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("要求失敗：{e}"))?;

    // Read until the server closes the connection. A timeout after some data has arrived is not
    // fatal — keep whatever we got and try to parse it.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > 4 * 1024 * 1024 {
                    break;
                }
            }
            Err(_) if !raw.is_empty() => break,
            Err(e) => return Err(format!("讀取回應失敗：{e}")),
        }
    }
    let response = String::from_utf8_lossy(&raw).into_owned();
    let body = response.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or(&response);
    Ok(dechunk(body))
}

/// Undoes `Transfer-Encoding: chunked` framing when the server used it.
fn dechunk(body: &str) -> String {
    let trimmed = body.trim_start();
    // A chunked body starts with a hex length line; a JSON list starts with '[' or '{'.
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        return body.to_string();
    }
    let mut out = String::new();
    let mut rest = trimmed;
    while let Some((len_line, after)) = rest.split_once("\r\n") {
        let Ok(len) = usize::from_str_radix(len_line.split(';').next().unwrap_or("").trim(), 16) else {
            return body.to_string(); // not chunked after all
        };
        if len == 0 || after.len() < len {
            break;
        }
        out.push_str(&after[..len]);
        rest = after[len..].trim_start_matches("\r\n");
    }
    if out.is_empty() {
        body.to_string()
    } else {
        out
    }
}

pub fn parse_targets(body: &str) -> Option<Vec<Target>> {
    let json: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let arr = json.as_array()?;
    Some(
        arr.iter()
            .map(|t| {
                let field = |name: &str| t.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string();
                Target {
                    kind: field("type"),
                    url: field("url"),
                    ws_debugger_url: field("webSocketDebuggerUrl"),
                }
            })
            .collect(),
    )
}

/// Picks the page target to inspect: the first `page` whose URL matches, else the first page.
pub fn pick_target<'a>(targets: &'a [Target], page_url: &str) -> Option<&'a Target> {
    let pages = || targets.iter().filter(|t| t.kind == "page" && !t.ws_debugger_url.is_empty());
    pages().find(|t| t.url == page_url).or_else(|| pages().next())
}

/// The URL to load in the docked DevTools WebView.
///
/// Always the runtime's own bundled `devtools_app.html`, never the server's `devtoolsFrontendUrl`:
/// newer runtimes point that at a hosted aka.ms copy, which redirects to a web search, and older
/// ones at `inspector.html`, the remote-debugging variant that splits off half the pane for a
/// screencast of the page we are already showing.
pub fn frontend_url(port: u16, target: &Target) -> Option<String> {
    // "ws://127.0.0.1:PORT/devtools/page/ID" -> "127.0.0.1:PORT/devtools/page/ID"
    let ws = target.ws_debugger_url.strip_prefix("ws://")?;
    Some(format!("http://127.0.0.1:{port}/devtools/devtools_app.html?ws={ws}"))
}

/// The browser switches that enable the remote-debugging server.
pub fn browser_switches() -> &'static str {
    "--remote-debugging-port=0 --remote-allow-origins=*"
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"[
        {"type":"page","url":"https://github.com/","webSocketDebuggerUrl":"ws://127.0.0.1:51234/devtools/page/AAA",
         "devtoolsFrontendUrl":"/devtools/inspector.html?ws=127.0.0.1:51234/devtools/page/AAA"},
        {"type":"page","url":"https://app.clickup.com/","webSocketDebuggerUrl":"ws://127.0.0.1:51234/devtools/page/BBB"},
        {"type":"service_worker","url":"https://x/sw.js","webSocketDebuggerUrl":"ws://127.0.0.1:51234/devtools/page/CCC"}
    ]"#;

    #[test]
    fn parses_and_picks() {
        let targets = parse_targets(BODY).unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(pick_target(&targets, "https://app.clickup.com/").unwrap().url, "https://app.clickup.com/");
        // An unknown URL falls back to the first page, never a service worker.
        let t = pick_target(&targets, "https://unknown/").unwrap();
        assert_eq!(t.kind, "page");
        assert_eq!(t.url, "https://github.com/");
    }

    #[test]
    fn frontend_is_the_local_devtools_app() {
        let targets = parse_targets(BODY).unwrap();
        // With a root-relative inspector.html (screencast) devtoolsFrontendUrl, and without one.
        for (target, id) in targets.iter().zip(["AAA", "BBB"]) {
            assert_eq!(
                frontend_url(51234, target).unwrap(),
                format!("http://127.0.0.1:51234/devtools/devtools_app.html?ws=127.0.0.1:51234/devtools/page/{id}")
            );
        }
    }

    #[test]
    fn hosted_frontend_url_is_ignored() {
        // What runtime 154 reports; loading it lands on a Bing search page, not DevTools.
        let body = r#"[{"type":"page","url":"https://a/","webSocketDebuggerUrl":"ws://127.0.0.1:1/devtools/page/A",
                        "devtoolsFrontendUrl":"https://aka.ms/docs-landing-page/serve_rev/@49f7/inspector.html?ws=127.0.0.1:1/devtools/page/A"}]"#;
        let targets = parse_targets(body).unwrap();
        assert_eq!(
            frontend_url(1, &targets[0]).unwrap(),
            "http://127.0.0.1:1/devtools/devtools_app.html?ws=127.0.0.1:1/devtools/page/A"
        );
    }

    #[test]
    fn dechunks_a_chunked_body() {
        let chunked = "2\r\n[]\r\n0\r\n\r\n";
        assert_eq!(dechunk(chunked), "[]");
        assert_eq!(dechunk("[1]"), "[1]");
    }

    #[test]
    fn non_json_is_reported_not_silently_dropped() {
        assert!(parse_targets("Host header is specified and is not an IP address or localhost.").is_none());
    }

    #[test]
    fn port_file() {
        let dir = std::env::temp_dir().join(format!("lb-devtools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("DevToolsActivePort"), "51899\n/devtools/browser/abc\n").unwrap();
        assert_eq!(read_active_port(&dir), Some(51899));
    }
}
