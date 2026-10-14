//! Tiny file logger. The release build has no console, so errors and panics go to
//! `litebrowser.log` in the data directory.
//!
//! Every line carries the source file and line that wrote it. `log!` always writes; `debug!` and
//! `trace!` only when the user raised the level in Settings, and skip formatting otherwise.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info = 0,
    /// What the user did and what pages did: actions, navigations, page errors, blocked requests.
    Debug = 1,
    /// Everything else that moves: every event, memory each tick, window messages, layout.
    Trace = 2,
}

impl Level {
    pub fn from_name(name: &str) -> Level {
        match name {
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => Level::Info,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

struct Log {
    file: File,
    path: PathBuf,
    len: u64,
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);
static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// A full log moves aside instead of being emptied: the run a user is reporting is usually the one
/// before it filled up. Checked while running too, since trace can fill it within one session.
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

fn open(path: &Path) -> Option<Log> {
    let append = |path: &Path| OpenOptions::new().create(true).append(true).open(path);
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len <= MAX_LOG_BYTES {
        return Some(Log { file: append(path).ok()?, path: path.to_path_buf(), len });
    }
    // Should the old copy be locked (open in an editor), start over rather than grow forever.
    let file = if std::fs::rename(path, path.with_extension("old.log")).is_ok() { append(path) } else { File::create(path) };
    Some(Log { file: file.ok()?, path: path.to_path_buf(), len: 0 })
}

pub fn init(path: &Path) {
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = open(path);
    std::panic::set_hook(Box::new(|info| {
        let (file, line) = info.location().map_or(("?", 0), |l| (l.file(), l.line()));
        write_line(Level::Info, file, line, &format!("PANIC: {info}"));
    }));
}

pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    level as u8 <= LEVEL.load(Ordering::Relaxed)
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

pub fn write_line(level: Level, file: &str, line: u32, msg: &str) {
    let file = file.strip_prefix("src/").or_else(|| file.strip_prefix("src\\")).unwrap_or(file);
    let text = format!("[{}] {} {file}:{line} {msg}\n", timestamp(), level.tag());
    if cfg!(debug_assertions) {
        eprint!("{text}");
    }
    let mut guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(log) = guard.as_mut() {
        let _ = log.file.write_all(text.as_bytes());
        log.len += text.len() as u64;
        if log.len > MAX_LOG_BYTES {
            let path = log.path.clone();
            *guard = open(&path);
        }
    }
}

#[allow(unused_macros)]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::logging::write_line($crate::logging::Level::Info, file!(), line!(), &format!($($arg)*))
    };
}
#[allow(unused_macros)]
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::logging::enabled($crate::logging::Level::Debug) {
            $crate::logging::write_line($crate::logging::Level::Debug, file!(), line!(), &format!($($arg)*))
        }
    };
}
#[allow(unused_macros)]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::logging::enabled($crate::logging::Level::Trace) {
            $crate::logging::write_line($crate::logging::Level::Trace, file!(), line!(), &format!($($arg)*))
        }
    };
}
#[allow(unused_imports)]
pub(crate) use {debug, log, trace};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_gate_by_threshold() {
        // Info always passes; debug and trace only up to the chosen level.
        set_level(Level::Debug);
        assert!(enabled(Level::Info) && enabled(Level::Debug) && !enabled(Level::Trace));
        set_level(Level::Info);
        assert!(!enabled(Level::Debug));
        assert_eq!(Level::from_name("trace"), Level::Trace);
        assert_eq!(Level::from_name("nonsense"), Level::Info, "a typo in config.toml must not turn logging off");
    }

    #[test]
    fn a_full_log_rolls_over_to_old_log() {
        let dir = std::env::temp_dir().join(format!("lb-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("litebrowser.log");
        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        let log = open(&path).unwrap();
        assert_eq!(log.len, 0, "the new log starts empty");
        assert_eq!(std::fs::metadata(dir.join("litebrowser.old.log")).unwrap().len(), MAX_LOG_BYTES + 1);
    }
}
