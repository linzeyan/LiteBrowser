//! Tiny file logger. The release build has no console, so errors and panics go to
//! `litebrowser.log` in the data directory.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

static LOG: Mutex<Option<File>> = Mutex::new(None);

const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn init(path: &Path) {
    // A full log moves aside instead of being emptied: the run a user is reporting is usually the
    // one before the restart that crossed the limit.
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("old.log"));
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = file;

    std::panic::set_hook(Box::new(|info| {
        write_line(&format!("PANIC: {info}"));
    }));
}

/// Local wall-clock time to the millisecond, so a line can be matched with what the user saw.
#[cfg(windows)]
fn timestamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

#[cfg(not(windows))]
fn timestamp() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

pub fn write_line(msg: &str) {
    let line = format!("[{}] {msg}\n", timestamp());
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
