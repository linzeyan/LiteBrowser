//! Turns what the user typed into the address bar into a URL to load.

/// Returns the URL to navigate to, or `None` for empty input.
pub fn to_url(input: &str, search_url: &str) -> Option<String> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    if has_scheme(text) {
        return Some(text.to_string());
    }
    if !text.chars().any(char::is_whitespace) {
        let host = host_part(text);
        if is_local_host(host) {
            return Some(format!("http://{text}"));
        }
        if looks_like_domain(host) {
            return Some(format!("https://{text}"));
        }
    }
    Some(search_url.replace("{}", &encode_query(text)))
}

fn has_scheme(text: &str) -> bool {
    if text.contains("://") {
        let scheme = &text[..text.find("://").unwrap_or(0)];
        return !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    }
    const SCHEMES: &[&str] = &["about:", "data:", "file:", "mailto:", "view-source:", "edge:", "javascript:"];
    let lower = text.to_ascii_lowercase();
    SCHEMES.iter().any(|s| lower.starts_with(s))
}

/// `host[:port]` part of `example.com:8080/path?q`.
fn host_part(text: &str) -> &str {
    let end = text.find(['/', '?', '#']).unwrap_or(text.len());
    &text[..end]
}

fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host; // IPv6 literal
    }
    match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    }
}

fn is_local_host(host: &str) -> bool {
    let h = strip_port(host).to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost") || is_ipv4(&h) || h.starts_with('[')
}

fn is_ipv4(h: &str) -> bool {
    let parts: Vec<&str> = h.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
}

fn looks_like_domain(host: &str) -> bool {
    let h = strip_port(host);
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return false;
    }
    let tld = labels[labels.len() - 1];
    let valid_chars = labels.iter().all(|l| l.chars().all(|c| c.is_alphanumeric() || c == '-'));
    valid_chars && tld.chars().count() >= 2 && tld.chars().all(char::is_alphabetic)
}

/// Percent-encodes a search query (`application/x-www-form-urlencoded` style).
pub fn encode_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Host name of a URL, lowercased, without port or credentials.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = strip_port(authority).trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// How the omnibox shows a URL while it is not being edited: no http(s) scheme (the site icon
/// already tells them apart) and no lone trailing slash, as Chrome and Firefox show it.
pub fn display(url: &str) -> String {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    match rest.strip_suffix('/') {
        Some(host) if !host.contains('/') => host.to_string(),
        _ => rest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "https://www.google.com/search?q={}";

    fn url(s: &str) -> String {
        to_url(s, S).unwrap()
    }

    #[test]
    fn keeps_full_urls() {
        assert_eq!(url("https://github.com/a/b"), "https://github.com/a/b");
        assert_eq!(url("  http://x.y  "), "http://x.y");
        assert_eq!(url("about:blank"), "about:blank");
        assert_eq!(url("file:///C:/tmp/a.html"), "file:///C:/tmp/a.html");
    }

    #[test]
    fn adds_https_to_domains() {
        assert_eq!(url("github.com"), "https://github.com");
        assert_eq!(url("app.clickup.com/123/home"), "https://app.clickup.com/123/home");
        assert_eq!(url("claude.ai"), "https://claude.ai");
        assert_eq!(url("example.com:8443/x"), "https://example.com:8443/x");
    }

    #[test]
    fn local_hosts_use_http() {
        assert_eq!(url("localhost:3000"), "http://localhost:3000");
        assert_eq!(url("192.168.1.10/admin"), "http://192.168.1.10/admin");
    }

    #[test]
    fn everything_else_is_a_search() {
        assert_eq!(url("rust slint"), "https://www.google.com/search?q=rust+slint");
        assert_eq!(url("hello"), "https://www.google.com/search?q=hello");
        assert_eq!(url("1.5"), "https://www.google.com/search?q=1.5");
        assert_eq!(url("記憶體"), "https://www.google.com/search?q=%E8%A8%98%E6%86%B6%E9%AB%94");
        assert_eq!(url("what is a.b c"), "https://www.google.com/search?q=what+is+a.b+c");
    }

    #[test]
    fn empty_input() {
        assert_eq!(to_url("   ", S), None);
    }

    #[test]
    fn display_hides_only_the_scheme_and_a_lone_slash() {
        // Whatever identifies the page stays visible; only what the site icon already says goes.
        assert_eq!(display("https://github.com/"), "github.com");
        assert_eq!(display("http://example.com/a/"), "example.com/a/");
        assert_eq!(display("https://www.google.com/search?q=x"), "www.google.com/search?q=x");
        assert_eq!(display("file:///C:/a.html"), "file:///C:/a.html");
        assert_eq!(display(""), "");
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://Ads.Example.com:443/x?y").as_deref(), Some("ads.example.com"));
        assert_eq!(host_of("https://user:pw@example.com/").as_deref(), Some("example.com"));
        assert_eq!(host_of("about:blank"), None);
    }
}
