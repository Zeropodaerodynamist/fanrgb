//! Minimal file logger that also keeps recent lines in memory for the UI.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use log::{Level, LevelFilter, Log, Metadata, Record};

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
const RECENT_LINES: usize = 400;

struct Logger {
    file: Mutex<Option<File>>,
    path: PathBuf,
    /// Bytes in the current log file; rotated at run time when it exceeds `MAX_LOG_BYTES` (QA-6).
    written: AtomicU64,
    recent: Mutex<VecDeque<String>>,
    level: LevelFilter,
    echo_stderr: bool,
}

static LOGGER: std::sync::OnceLock<Logger> = std::sync::OnceLock::new();

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level && (metadata.target().starts_with("fanrgb") || metadata.level() <= Level::Warn)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = chrono_like_now();
        // Token-shaped text never reaches the log file, the in-app log or the diagnostics report.
        let msg = crate::updater::token::redact(&record.args().to_string());
        let line = format!("{now} {:<5} {}", record.level(), escape_controls(&msg));
        if let Ok(mut f) = self.file.lock()
            && let Some(file) = f.as_mut()
        {
            let _ = writeln!(file, "{line}");
            let n = self.written.fetch_add(line.len() as u64 + 1, Ordering::Relaxed) + line.len() as u64 + 1;
            if n > MAX_LOG_BYTES {
                // Keep one previous file, like at start-up.
                *f = None;
                let _ = std::fs::rename(&self.path, self.path.with_extension("old.log"));
                *f = open_log(&self.path);
                self.written.store(0, Ordering::Relaxed);
            }
        }
        if self.echo_stderr {
            eprintln!("{line}");
        }
        if let Ok(mut r) = self.recent.lock() {
            if r.len() >= RECENT_LINES {
                r.pop_front();
            }
            r.push_back(line);
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock()
            && let Some(file) = f.as_mut()
        {
            let _ = file.flush();
        }
    }
}

/// Escapes CR, LF and other control characters (except TAB) so a logged external string (fan name,
/// path, model key) cannot forge extra lines in the log or the diagnostics report.
pub fn escape_controls(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(|c| c.is_control() && c != '\t') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push('\t'),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Local wall-clock time as `YYYY-MM-DD HH:MM:SS` without pulling in a date crate.
fn chrono_like_now() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: SYSTEMTIME is plain data; all-zero is a valid value.
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid, writable SYSTEMTIME.
    unsafe { GetLocalTime(&mut st) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond)
}

pub fn init(verbose: bool, echo_stderr: bool) {
    // Elevated: %ProgramData%\FanRGB (admin-only, see paths.rs); otherwise the user's own folder.
    let path = super::paths::log_file();
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            // Never append through a link (cannot happen in the admin-only folder; defence in depth).
            let _ = std::fs::remove_file(&path);
        } else if meta.len() > MAX_LOG_BYTES {
            let _ = std::fs::rename(&path, path.with_extension("old.log"));
        }
    }
    let file = open_log(&path);
    let size = file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
    let level = if verbose { LevelFilter::Debug } else { LevelFilter::Info };
    let logger = LOGGER.get_or_init(|| Logger {
        file: Mutex::new(file),
        path: path.clone(),
        written: AtomicU64::new(size),
        recent: Mutex::new(VecDeque::new()),
        level,
        echo_stderr,
    });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
    for (lvl, msg) in super::paths::take_init_notes() {
        log::log!(lvl, "{msg}");
    }
}

fn open_log(path: &Path) -> Option<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_OPEN_REPARSE_POINT: open the name itself, never a link target.
    OpenOptions::new().create(true).append(true).custom_flags(0x0020_0000).open(path).ok()
}

/// The last `n` lines of the log file, which also holds what the running app logged when `--diag`
/// writes the report; this process's recent lines if the file can't be read.
pub fn tail(n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let path = LOGGER.get().map(|l| l.path.clone()).unwrap_or_else(super::paths::log_file);
    let read = || -> std::io::Result<String> {
        let mut f = File::open(&path)?;
        let len = f.metadata()?.len();
        let start = len.saturating_sub(128 * 1024);
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        // Drop a partial first line.
        Ok(if start > 0 { text.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default() } else { text })
    };
    let lines: Vec<String> = match read() {
        Ok(t) => t.lines().map(|l| escape_controls(&crate::updater::token::redact(l)).into_owned()).collect(),
        Err(_) => recent_lines(),
    };
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Most recent log lines (oldest first).
pub fn recent_lines() -> Vec<String> {
    LOGGER.get().and_then(|l| l.recent.lock().ok().map(|r| r.iter().cloned().collect())).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_cannot_forge_log_lines() {
        assert_eq!(escape_controls("CPU fan"), "CPU fan");
        assert_eq!(escape_controls("a\r\n2026-01-01 00:00:00 ERROR forged"), "a\\r\\n2026-01-01 00:00:00 ERROR forged");
        assert_eq!(escape_controls("x\u{1b}[31m\ty"), "x\\u{1b}[31m\ty");
        assert!(!escape_controls("line1\nline2").contains('\n'));
    }
}
