//! Where LiteBrowser keeps its files.
//!
//! Lookup order for the data directory:
//! 1. `LITEBROWSER_DATA` environment variable
//! 2. `<exe dir>\data` when `<exe dir>\portable.txt` or `<exe dir>\data` exists (portable mode)
//! 3. `%LOCALAPPDATA%\LiteBrowser`
//! 4. `.\LiteBrowserData` as a last resort

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn detect() -> Self {
        let root = data_root();
        let _ = std::fs::create_dir_all(&root);
        let paths = Self { root };
        let _ = std::fs::create_dir_all(paths.extensions_dir());
        paths
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn bookmarks(&self) -> PathBuf {
        self.root.join("bookmarks.json")
    }
    pub fn history(&self) -> PathBuf {
        self.root.join("history.json")
    }
    pub fn session(&self) -> PathBuf {
        self.root.join("session.json")
    }
    pub fn zoom(&self) -> PathBuf {
        self.root.join("zoom.json")
    }
    pub fn blocklist(&self) -> PathBuf {
        self.root.join("blocklist.txt")
    }
    /// Where the current MCP endpoint URL (including its per-run token) is written.
    pub fn mcp_url(&self) -> PathBuf {
        self.root.join("mcp-url.txt")
    }
    pub fn log(&self) -> PathBuf {
        self.root.join("litebrowser.log")
    }
    pub fn extensions_dir(&self) -> PathBuf {
        self.root.join("extensions")
    }
    /// WebView2 user data folder (cookies, cache, saved passwords).
    pub fn webview_data(&self) -> PathBuf {
        self.root.join("webview2")
    }
}

fn data_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("LITEBROWSER_DATA").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        let data = exe_dir.join("data");
        if exe_dir.join("portable.txt").exists() || data.is_dir() {
            return data;
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        return PathBuf::from(local).join("LiteBrowser");
    }
    PathBuf::from("LiteBrowserData")
}

/// Writes `contents` to `path` via a temp file + rename so a crash never leaves a half-written file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}
