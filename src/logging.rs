//! Tiny file logger. The release build has no console, so errors and panics go to
//! `litebrowser.log` in the data directory.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: Mutex<Option<File>> = Mutex::new(None);

const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn init(path: &Path) {
    // Start over when the log grows too big; old entries are rarely useful.
    let truncate = std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(!truncate)
        .truncate(truncate)
        .open(path)
        .ok();
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = file;

    std::panic::set_hook(Box::new(|info| {
        write_line(&format!("PANIC: {info}"));
    }));
}

pub fn write_line(msg: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let line = format!("[{secs}] {msg}\n");
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
    if let Some(file) = LOG.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        let _ = file.write_all(line.as_bytes());
    }
}

#[allow(unused_macros)]
macro_rules! log {
    ($($arg:tt)*) => { $crate::logging::write_line(&format!($($arg)*)) };
}
#[allow(unused_imports)]
pub(crate) use log;
