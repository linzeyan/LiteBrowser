//! An MCP server so an LLM can drive the browser.
//!
//! Speaks JSON-RPC 2.0 over HTTP POST, on localhost unless the user picks another address. The
//! request path carries a token generated at startup, so another program cannot drive the browser
//! by guessing the port.
//!
//! The HTTP side lives on its own thread; every tool call is handed to the UI thread through
//! `dispatch`, because WebView2 may only be touched there.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::{json, Value};

/// What an LLM asked the browser to do. Executed on the UI thread.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    ListTabs,
    NewTab { url: Option<String>, activate: bool },
    CloseTab { tab: u64 },
    ActivateTab { tab: u64 },
    Navigate { tab: Option<u64>, url: String },
    Back { tab: Option<u64> },
    Forward { tab: Option<u64> },
    Reload { tab: Option<u64> },
    PageText { tab: Option<u64> },
    PageHtml { tab: Option<u64> },
    ExecuteJs { tab: Option<u64>, script: String },
    Screenshot { tab: Option<u64> },
}

/// Runs a [`Call`] and returns its result, or a message explaining why it could not run.
pub type Dispatcher = Arc<dyn Fn(Call) -> Result<Value, String> + Send + Sync>;

pub const SERVER_NAME: &str = "litebrowser";
const DEFAULT_PROTOCOL: &str = "2025-06-18";

/// Starts the server on `addr` (port 0 picks a free one) and returns the address it actually bound.
/// The listener thread runs until the process exits.
pub fn serve(addr: SocketAddr, token: String, dispatch: Dispatcher) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let token = token.clone();
            let dispatch = dispatch.clone();
            // One connection at a time is plenty for a single LLM session, but a slow page should
            // not wedge the listener, so each connection gets its own thread.
            std::thread::spawn(move || handle_connection(stream, &token, &dispatch));
        }
    });
    Ok(bound)
}

fn handle_connection(stream: TcpStream, token: &str, dispatch: &Dispatcher) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut out = stream;
    let Some(request) = read_request(&mut reader) else {
        let _ = write_response(&mut out, 400, "application/json", b"{}");
        return;
    };
    if request.path.trim_end_matches('/') != format!("/mcp/{token}") {
        let _ = write_response(&mut out, 404, "text/plain", b"not found");
        return;
    }
    if request.method != "POST" {
        // A GET is how some clients probe the endpoint; say it is alive but needs POST.
        let _ = write_response(&mut out, 405, "text/plain", b"POST JSON-RPC to this URL");
        return;
    }
    let response = match serde_json::from_str::<Value>(&request.body) {
        Ok(message) => handle_message(&message, dispatch),
        Err(e) => Some(error_response(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    match response {
        // Notifications get no body, just an acknowledgement.
        None => {
            let _ = write_response(&mut out, 202, "application/json", b"");
        }
        Some(value) => {
            let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
            let _ = write_response(&mut out, 200, "application/json", &body);
        }
    }
}

struct Request {
    method: String,
    path: String,
    body: String,
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut start = String::new();
    reader.read_line(&mut start).ok()?;
    let mut parts = start.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length.min(16 * 1024 * 1024)];
    if !body.is_empty() {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request { method, path, body: String::from_utf8_lossy(&body).into_owned() })
}

fn write_response(out: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    out.write_all(header.as_bytes())?;
    out.write_all(body)?;
    out.flush()
}

/// Handles one JSON-RPC message. Returns `None` for notifications (no reply expected).
pub fn handle_message(message: &Value, dispatch: &Dispatcher) -> Option<Value> {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(json!({}));
    let is_notification = message.get("id").is_none();

    let result = match method {
        "initialize" => {
            let protocol = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL)
                .to_string();
            Ok(json!({
                "protocolVersion": protocol,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(&params, dispatch),
        _ if is_notification => return None,
        _ => Err((-32601, format!("unknown method: {method}"))),
    };

    if is_notification {
        return None;
    }
    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, message)) => error_response(id, code, &message),
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Runs a `tools/call`. Tool failures are reported as `isError` results, not JSON-RPC errors,
/// so the model sees the message and can adjust.
fn call_tool(params: &Value, dispatch: &Dispatcher) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let tab = args.get("tab_id").and_then(Value::as_u64);
    let text_arg = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);

    let call = match name {
        "list_tabs" => Call::ListTabs,
        "new_tab" => Call::NewTab {
            url: text_arg("url"),
            activate: args.get("activate").and_then(Value::as_bool).unwrap_or(true),
        },
        "close_tab" => match tab {
            Some(tab) => Call::CloseTab { tab },
            None => return Ok(tool_error("close_tab needs tab_id")),
        },
        "activate_tab" => match tab {
            Some(tab) => Call::ActivateTab { tab },
            None => return Ok(tool_error("activate_tab needs tab_id")),
        },
        "navigate" => match text_arg("url") {
            Some(url) => Call::Navigate { tab, url },
            None => return Ok(tool_error("navigate needs url")),
        },
        "go_back" => Call::Back { tab },
        "go_forward" => Call::Forward { tab },
        "reload" => Call::Reload { tab },
        "get_page_text" => Call::PageText { tab },
        "get_page_html" => Call::PageHtml { tab },
        "execute_js" => match text_arg("script") {
            Some(script) => Call::ExecuteJs { tab, script },
            None => return Ok(tool_error("execute_js needs script")),
        },
        "screenshot" => Call::Screenshot { tab },
        other => return Err((-32602, format!("unknown tool: {other}"))),
    };

    let is_screenshot = matches!(call, Call::Screenshot { .. });
    Ok(match dispatch(call) {
        Ok(value) if is_screenshot => {
            let data = value.get("png_base64").and_then(Value::as_str).unwrap_or_default();
            json!({ "content": [{ "type": "image", "data": data, "mimeType": "image/png" }] })
        }
        Ok(value) => {
            let text = value.as_str().map(str::to_string).unwrap_or_else(|| {
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
            });
            json!({ "content": [{ "type": "text", "text": text }] })
        }
        Err(message) => tool_error(&message),
    })
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

/// Tool list. `tab_id` is optional everywhere it appears: the active tab is used when omitted.
pub fn tool_definitions() -> Vec<Value> {
    let tab_id = json!({ "type": "integer", "description": "Tab id from list_tabs; the active tab when omitted" });
    let no_args = |name: &str, description: &str, with_tab: bool| {
        let properties = if with_tab { json!({ "tab_id": tab_id }) } else { json!({}) };
        json!({
            "name": name,
            "description": description,
            "inputSchema": { "type": "object", "properties": properties },
        })
    };
    vec![
        no_args("list_tabs", "List the open tabs with their id, title, URL and which one is active.", false),
        json!({
            "name": "new_tab",
            "description": "Open a new tab, optionally at a URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to open; a blank tab when omitted" },
                    "activate": { "type": "boolean", "description": "Switch to the new tab (default true)" },
                },
            },
        }),
        json!({
            "name": "close_tab",
            "description": "Close a tab by id.",
            "inputSchema": { "type": "object", "properties": { "tab_id": tab_id }, "required": ["tab_id"] },
        }),
        json!({
            "name": "activate_tab",
            "description": "Bring a tab to the foreground by id.",
            "inputSchema": { "type": "object", "properties": { "tab_id": tab_id }, "required": ["tab_id"] },
        }),
        json!({
            "name": "navigate",
            "description": "Load a URL in a tab.",
            "inputSchema": {
                "type": "object",
                "properties": { "url": { "type": "string" }, "tab_id": tab_id },
                "required": ["url"],
            },
        }),
        no_args("go_back", "Go back in a tab's history.", true),
        no_args("go_forward", "Go forward in a tab's history.", true),
        no_args("reload", "Reload a tab.", true),
        no_args("get_page_text", "Get the visible text of a page (document.body.innerText).", true),
        no_args("get_page_html", "Get a page's full HTML.", true),
        json!({
            "name": "execute_js",
            "description": "Run JavaScript in a page and return its result as JSON. Use this to click \
                            elements or fill forms, e.g. document.querySelector('#id').click().",
            "inputSchema": {
                "type": "object",
                "properties": { "script": { "type": "string" }, "tab_id": tab_id },
                "required": ["script"],
            },
        }),
        no_args(
            "screenshot",
            "Capture a PNG screenshot of the tab on screen. Background tabs cannot be captured; activate_tab first.",
            true,
        ),
    ]
}

/// A URL-safe random token for the endpoint path.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Still unique per run, just less random.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes.copy_from_slice(&nanos.to_le_bytes().repeat(2)[..16]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Mutex;

    /// Records the calls it receives and answers with a canned value.
    fn recorder() -> (Dispatcher, Arc<Mutex<Vec<Call>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let dispatch: Dispatcher = Arc::new(move |call| {
            sink.lock().unwrap().push(call.clone());
            match call {
                Call::Screenshot { .. } => Ok(json!({ "png_base64": "aGk=" })),
                Call::PageText { .. } => Ok(json!("hello page")),
                _ => Ok(json!({ "ok": true })),
            }
        });
        (dispatch, seen)
    }

    fn call(name: &str, arguments: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": { "name": name, "arguments": arguments } })
    }

    #[test]
    fn initialize_echoes_the_protocol_and_advertises_tools() {
        let (dispatch, _) = recorder();
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                          "params": { "protocolVersion": "2024-11-05" } });
        let reply = handle_message(&msg, &dispatch).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(reply["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(reply["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_get_no_reply() {
        let (dispatch, _) = recorder();
        let msg = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_message(&msg, &dispatch).is_none());
    }

    #[test]
    fn every_advertised_tool_is_dispatchable() {
        let (dispatch, seen) = recorder();
        let tools = tool_definitions();
        assert!(!tools.is_empty());
        for tool in &tools {
            let name = tool["name"].as_str().unwrap();
            // Supply the arguments each tool requires.
            let args = match name {
                "close_tab" | "activate_tab" => json!({ "tab_id": 3 }),
                "navigate" => json!({ "url": "https://example.com" }),
                "execute_js" => json!({ "script": "1+1" }),
                _ => json!({}),
            };
            let reply = handle_message(&call(name, args), &dispatch).unwrap();
            assert!(reply.get("error").is_none(), "{name} returned a JSON-RPC error: {reply}");
            assert_ne!(reply["result"]["isError"], json!(true), "{name} failed: {reply}");
        }
        assert_eq!(seen.lock().unwrap().len(), tools.len());
    }

    #[test]
    fn arguments_reach_the_dispatcher() {
        let (dispatch, seen) = recorder();
        handle_message(&call("navigate", json!({ "url": "https://a.com", "tab_id": 2 })), &dispatch);
        handle_message(&call("new_tab", json!({ "url": "https://b.com", "activate": false })), &dispatch);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], Call::Navigate { tab: Some(2), url: "https://a.com".into() });
        assert_eq!(seen[1], Call::NewTab { url: Some("https://b.com".into()), activate: false });
    }

    #[test]
    fn missing_required_arguments_are_tool_errors_not_crashes() {
        let (dispatch, seen) = recorder();
        let reply = handle_message(&call("navigate", json!({})), &dispatch).unwrap();
        assert_eq!(reply["result"]["isError"], json!(true));
        assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains("url"));
        assert!(seen.lock().unwrap().is_empty(), "nothing should reach the browser");
    }

    #[test]
    fn a_failing_call_is_reported_to_the_model() {
        let dispatch: Dispatcher = Arc::new(|_| Err("no such tab".into()));
        let reply = handle_message(&call("reload", json!({ "tab_id": 99 })), &dispatch).unwrap();
        assert_eq!(reply["result"]["isError"], json!(true));
        assert_eq!(reply["result"]["content"][0]["text"], "no such tab");
    }

    #[test]
    fn text_and_image_results_are_shaped_for_mcp() {
        let (dispatch, _) = recorder();
        let text = handle_message(&call("get_page_text", json!({})), &dispatch).unwrap();
        assert_eq!(text["result"]["content"][0]["type"], "text");
        assert_eq!(text["result"]["content"][0]["text"], "hello page");

        let shot = handle_message(&call("screenshot", json!({})), &dispatch).unwrap();
        assert_eq!(shot["result"]["content"][0]["type"], "image");
        assert_eq!(shot["result"]["content"][0]["mimeType"], "image/png");
        assert_eq!(shot["result"]["content"][0]["data"], "aGk=");
    }

    #[test]
    fn unknown_methods_and_tools_are_rejected() {
        let (dispatch, _) = recorder();
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": "nope" });
        assert_eq!(handle_message(&msg, &dispatch).unwrap()["error"]["code"], -32601);
        let reply = handle_message(&call("fly", json!({})), &dispatch).unwrap();
        assert_eq!(reply["error"]["code"], -32602);
    }

    #[test]
    fn tokens_are_unique_and_url_safe() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_endpoint_requires_the_token_and_answers_json_rpc() {
        let (dispatch, _) = recorder();
        let token = generate_token();
        let addr = serve((Ipv4Addr::LOCALHOST, 0).into(), token.clone(), dispatch).unwrap();

        let post = |path: &str, body: &str| -> String {
            let mut stream = TcpStream::connect(addr).unwrap();
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(request.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        };

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let good = post(&format!("/mcp/{token}"), body);
        assert!(good.starts_with("HTTP/1.1 200"), "{good}");
        assert!(good.contains("list_tabs"));

        let bad = post("/mcp/wrong-token", body);
        assert!(bad.starts_with("HTTP/1.1 404"), "{bad}");
    }
}
