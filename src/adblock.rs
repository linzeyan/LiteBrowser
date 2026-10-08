//! Domain based request blocking.
//!
//! Deliberately simple: a set of domains, matched against the request host and its parent
//! domains. Each domain becomes a WebView2 request filter, so only matching requests ever reach
//! our code. For full EasyList-style blocking, install uBlock Origin Lite as an extension.

use std::collections::BTreeSet;
use std::path::Path;

/// Ad networks and trackers that none of the sites we care about need to work.
const BUILTIN: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "adservice.google.com",
    "google-analytics.com",
    "amazon-adsystem.com",
    "adnxs.com",
    "adsrvr.org",
    "criteo.com",
    "criteo.net",
    "taboola.com",
    "outbrain.com",
    "scorecardresearch.com",
    "quantserve.com",
    "moatads.com",
    "rubiconproject.com",
    "pubmatic.com",
    "openx.net",
    "casalemedia.com",
    "smartadserver.com",
    "advertising.com",
    "adform.net",
    "bidswitch.net",
    "yieldmo.com",
    "33across.com",
    "hotjar.com",
    "mouseflow.com",
    "ads-twitter.com",
    "ads.linkedin.com",
    "bat.bing.com",
];

#[derive(Clone, Debug, Default)]
pub struct Blocklist {
    domains: BTreeSet<String>,
}

impl Blocklist {
    pub fn builtin() -> Self {
        let mut list = Self::default();
        list.domains.extend(BUILTIN.iter().map(|d| d.to_string()));
        list
    }

    /// Built-in list plus the user's `blocklist.txt`.
    /// Accepts plain domains or hosts-file lines (`0.0.0.0 ads.example.com`); `#` starts a comment.
    pub fn load(path: &Path) -> Self {
        let mut list = Self::builtin();
        if let Ok(text) = std::fs::read_to_string(path) {
            list.add_lines(&text);
        } else {
            let _ = std::fs::write(
                path,
                "# 每行一個要封鎖的網域，例如：\n# ads.example.com\n# 也接受 hosts 檔格式：0.0.0.0 ads.example.com\n",
            );
        }
        list
    }

    fn add_lines(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let domain = line.split_whitespace().last().unwrap_or("");
            let domain = domain.trim_start_matches("*.").trim_matches('.').to_ascii_lowercase();
            if !domain.is_empty() && domain != "localhost" && domain.contains('.') {
                self.domains.insert(domain);
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.domains.len()
    }

    pub fn is_blocked_host(&self, host: &str) -> bool {
        let mut h = host.trim_end_matches('.');
        loop {
            if self.domains.contains(h) {
                return true;
            }
            match h.split_once('.') {
                Some((_, parent)) if parent.contains('.') => h = parent,
                _ => return false,
            }
        }
    }

    pub fn is_blocked_url(&self, url: &str) -> bool {
        crate::url_input::host_of(url).is_some_and(|h| self.is_blocked_host(&h))
    }

    /// WebView2 `AddWebResourceRequestedFilter` URI patterns covering every domain and its subdomains.
    pub fn filter_patterns(&self) -> Vec<String> {
        self.domains
            .iter()
            .flat_map(|d| [format!("*://{d}/*"), format!("*://*.{d}/*")])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_domain_and_subdomains() {
        let list = Blocklist::builtin();
        assert!(list.is_blocked_host("doubleclick.net"));
        assert!(list.is_blocked_host("stats.g.doubleclick.net"));
        assert!(!list.is_blocked_host("notdoubleclick.net"));
        assert!(!list.is_blocked_host("github.com"));
        assert!(!list.is_blocked_host("www.linkedin.com"));
        assert!(list.is_blocked_host("px.ads.linkedin.com"));
    }

    #[test]
    fn url_matching() {
        let list = Blocklist::builtin();
        assert!(list.is_blocked_url("https://www.google-analytics.com/collect?v=1"));
        assert!(!list.is_blocked_url("https://www.google.com/search?q=doubleclick.net"));
    }

    #[test]
    fn parses_user_lines() {
        let mut list = Blocklist::default();
        list.add_lines("# comment\n0.0.0.0 Tracker.Example.com\n*.ads.test.org  # trailing\n\nlocalhost\nnodot\n");
        assert!(list.is_blocked_host("tracker.example.com"));
        assert!(list.is_blocked_host("x.ads.test.org"));
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn patterns_cover_subdomains() {
        let mut list = Blocklist::default();
        list.add_lines("a.com");
        assert_eq!(list.filter_patterns(), vec!["*://a.com/*", "*://*.a.com/*"]);
    }
}
