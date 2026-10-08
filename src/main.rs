// No console window in release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// The core modules are only wired up to the UI on Windows; elsewhere they exist for the tests.
#![cfg_attr(not(windows), allow(dead_code))]

mod adblock;
mod config;
mod crx;
mod devtools;
mod import;
mod logging;
mod mcp;
mod paths;
mod shortcuts;
mod storage;
mod tabs;
mod update;
mod url_input;

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod favicon;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod net;
#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod webview;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    app::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("LiteBrowser runs on Windows only (it embeds Microsoft Edge WebView2).");
    std::process::exit(1);
}
