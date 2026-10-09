//! User settings stored in `config.toml`.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Search URL used when the address bar input is not a URL. `{}` is replaced by the query.
    pub search_url: String,
    /// Maximum number of tabs that keep a WebView (running or frozen), including the current tab.
    pub max_live_tabs: usize,
    /// A background tab is frozen (TrySuspend) after this many seconds.
    pub suspend_after_secs: u64,
    /// A background tab is discarded (WebView destroyed) after this many minutes.
    pub discard_after_mins: u64,
    /// When the whole browser uses more than this, the largest background tab is discarded.
    pub memory_budget_mb: u64,
    /// The tab limit and memory budget don't discard the two most recently used background tabs
    /// within this many minutes; they are only frozen.
    pub discard_grace_mins: u64,
    /// When the machine has less free memory than this, background tabs are discarded right away.
    pub low_memory_free_mb: u64,
    /// Block requests to known ad / tracker domains.
    pub adblock: bool,
    /// Pass `--disable-gpu` to WebView2. Saves a process, but some sites' bot checks need WebGL,
    /// so this is off by default. Needs a restart.
    pub disable_gpu: bool,
    /// Dock DevTools on the right instead of the bottom.
    pub devtools_dock_right: bool,
    /// Check GitHub for a newer release at startup and stage it for the next start.
    pub auto_update: bool,
    /// Serve an MCP endpoint so an LLM can drive the browser. Needs a restart.
    pub mcp_enabled: bool,
    /// IP address the MCP endpoint listens on: 127.0.0.1 = this machine only; 0.0.0.0 = every
    /// network interface, so anyone who can reach this machine and has the URL can drive the browser.
    pub mcp_host: String,
    /// Port for the MCP endpoint; 0 picks a free one.
    pub mcp_port: u16,
    /// Reopen the tabs from last time (as discarded tabs, so they cost no memory until opened).
    pub restore_session: bool,
    pub show_bookmarks_bar: bool,
    /// Extra Chromium command-line switches for WebView2. Needs a restart.
    pub extra_browser_args: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            search_url: "https://www.google.com/search?q={}".into(),
            max_live_tabs: 2,
            suspend_after_secs: 30,
            discard_after_mins: 15,
            memory_budget_mb: 500,
            discard_grace_mins: 5,
            low_memory_free_mb: 250,
            adblock: true,
            disable_gpu: false,
            devtools_dock_right: false,
            auto_update: true,
            mcp_enabled: false,
            mcp_host: "127.0.0.1".into(),
            mcp_port: 0,
            restore_session: true,
            show_bookmarks_bar: true,
            extra_browser_args: String::new(),
        }
    }
}

impl Config {
    /// Loads the config, writing a default one when the file is missing.
    /// A broken file falls back to defaults but is left untouched so the user can fix it.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text)
                .map(Config::sanitized)
                .map_err(|e| format!("config.toml 格式錯誤，已使用預設值：{e}")),
            Err(_) => {
                let cfg = Config::default();
                let _ = cfg.save(path);
                Ok(cfg)
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::paths::write_atomic(path, text.as_bytes())
    }

    pub fn sanitized(mut self) -> Self {
        self.max_live_tabs = self.max_live_tabs.clamp(1, 20);
        self.suspend_after_secs = self.suspend_after_secs.clamp(5, 24 * 3600);
        self.discard_after_mins = self.discard_after_mins.clamp(1, 24 * 60);
        self.memory_budget_mb = self.memory_budget_mb.clamp(100, 64 * 1024);
        self.discard_grace_mins = self.discard_grace_mins.min(24 * 60);
        self.low_memory_free_mb = self.low_memory_free_mb.clamp(50, 8 * 1024);
        if !self.search_url.contains("{}") {
            self.search_url = Config::default().search_url;
        }
        self.mcp_host = match self.mcp_host.trim().parse::<std::net::IpAddr>() {
            Ok(ip) => ip.to_string(),
            Err(_) => Config::default().mcp_host,
        };
        self
    }

    /// Command line switches handed to the WebView2 browser process.
    pub fn browser_args(&self) -> String {
        // msWebOOUI / msPdfOOUI: Edge "mini menu" popups that make no sense in an embedded browser.
        let mut args = vec!["--disable-features=msWebOOUI,msPdfOOUI".to_string()];
        if self.disable_gpu {
            // Plain --disable-gpu also takes WebGL away, and sites that fingerprint the canvas
            // (Cloudflare's bot check among them) then fail or loop. Allow the software renderer
            // so WebGL keeps working without a GPU process.
            args.push("--disable-gpu --enable-unsafe-swiftshader".into());
        }
        let extra = self.extra_browser_args.trim();
        if !extra.is_empty() {
            args.push(extra.to_string());
        }
        args.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_defaults() {
        let cfg: Config = toml::from_str("max_live_tabs = 3").unwrap();
        assert_eq!(cfg.max_live_tabs, 3);
        assert_eq!(cfg.memory_budget_mb, Config::default().memory_budget_mb);
    }

    #[test]
    fn roundtrip() {
        let cfg = Config { adblock: false, ..Config::default() };
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), cfg);
    }

    #[test]
    fn sanitize_clamps_and_fixes_search_url() {
        let cfg = Config { max_live_tabs: 0, search_url: "https://x".into(), ..Config::default() }.sanitized();
        assert_eq!(cfg.max_live_tabs, 1);
        assert!(cfg.search_url.contains("{}"));
    }

    #[test]
    fn mcp_host_must_be_an_ip_address() {
        // Anything that is not an IP falls back to this machine only, never to every interface.
        let host = |h: &str| Config { mcp_host: h.into(), ..Config::default() }.sanitized().mcp_host;
        assert_eq!(host(" 0.0.0.0 "), "0.0.0.0");
        assert_eq!(host("::1"), "::1");
        assert_eq!(host("example.com"), "127.0.0.1");
        assert_eq!(host(""), "127.0.0.1");
    }

    #[test]
    fn browser_args_respect_flags() {
        let cfg = Config { disable_gpu: false, extra_browser_args: " --foo ".into(), ..Config::default() };
        let args = cfg.browser_args();
        assert!(!args.contains("--disable-gpu"));
        assert!(args.ends_with("--foo"));

        // Disabling the GPU must keep software WebGL available.
        let cfg = Config { disable_gpu: true, ..Config::default() };
        let args = cfg.browser_args();
        assert!(args.contains("--disable-gpu"));
        assert!(args.contains("--enable-unsafe-swiftshader"));
    }
}
