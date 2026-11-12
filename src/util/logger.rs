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
        let line = format!("{now} {:<5} {}", record.level(), clean(&record.args().to_string()));
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

/// What a message becomes before it reaches the log file, stderr, the in-app log or a report.
/// Token-shaped text is masked. The Windows account name in profile paths and the machine part of
/// account SIDs are removed, as in the report: users attach `fanrgb.log` to public issues
/// (docs/TESTING.md). Control characters are escaped.
fn clean(msg: &str) -> String {
    let msg = crate::updater::token::redact(msg);
    escape_controls(&crate::diag::scrub_personal(&msg)).into_owned()
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
        // Lines written by an older build may still name the account; scrub them on the way out.
        Ok(t) => t.lines().map(clean).collect(),
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

    /// Hardware test, 12 Nov: the Dynamic Lighting line logged the desktop user's full SID, and
    /// docs/TESTING.md asks testers to attach fanrgb.log to a public issue.
    #[test]
    fn log_lines_do_not_name_the_user() {
        let line = clean(
            "Dynamic Lighting: reading the desktop user's settings in \
             HKEY_USERS\\S-1-5-21-1004336348-1177238915-682003330-1001; settings in \
             C:\\Users\\aoife\\AppData\\Roaming\\FanRGB\nforged",
        );
        assert!(!line.contains("1004336348") && !line.to_lowercase().contains("aoife"), "{line}");
        assert!(line.contains("HKEY_USERS\\S-1-5-21-<machine>-1001"), "{line}");
        assert!(line.contains("C:\\Users\\<user>\\AppData\\Roaming\\FanRGB"), "{line}");
        assert!(!line.contains('\n'), "{line}");
        // Ordinary lines are untouched.
        let plain = "IT8689E PWM2 (mb.pwm1): taking control; state: Secure C:\\ProgramData\\FanRGB";
        assert_eq!(clean(plain), plain);
    }

    // ---- independent tester, round 5 (aefad81) ----

    /// `clean` keeps none of `secrets` (case-insensitive) and no line break.
    fn assert_clean(input: &str, secrets: &[&str]) -> String {
        let out = clean(input);
        let lower = out.to_lowercase();
        for s in secrets {
            assert!(!lower.contains(&s.to_lowercase()), "{s:?} survives in {out:?} (from {input:?})");
        }
        assert!(!out.contains('\n') && !out.contains('\r'), "{out:?}");
        out
    }

    /// Tester: SID shapes the account SID takes in Windows output: a domain account, a local account,
    /// the `_Classes` hive, several in one line, at the very end of a line, inside quotes and Debug
    /// output. The machine or domain part goes, the RID stays; well-known SIDs stay as they are.
    #[test]
    fn sid_shapes_lose_their_machine_part() {
        let m = ["3623811015", "3361044348", "30300820", "1004336348", "1177238915", "682003330"];
        let out = assert_clean("domain user S-1-5-21-3623811015-3361044348-30300820-1013 logged on", &m);
        assert!(out.contains("S-1-5-21-<machine>-1013 logged on"), "{out}");
        let out = assert_clean("HKEY_USERS\\S-1-5-21-1004336348-1177238915-682003330-1001", &m);
        assert!(out.ends_with("S-1-5-21-<machine>-1001"), "{out}");
        let out = assert_clean("hive S-1-5-21-1004336348-1177238915-682003330-1001_Classes opened", &m);
        assert!(out.contains("S-1-5-21-<machine>-1001_Classes opened"), "{out}");
        let two = "a S-1-5-21-1004336348-1177238915-682003330-500, b \"S-1-5-21-3623811015-3361044348-30300820-1013\"";
        let out = assert_clean(two, &m);
        assert!(out.contains("-<machine>-500,") && out.contains("\"S-1-5-21-<machine>-1013\""), "{out}");
        let dbg = format!("{:?}", "HKEY_USERS\\S-1-5-21-1004336348-1177238915-682003330-1001\\Software");
        assert_clean(&dbg, &m);
        // Well-known SIDs carry no machine part and must stay readable.
        for wk in ["S-1-5-18", "S-1-5-19", "S-1-5-20", "S-1-5-32-544", "S-1-1-0", "S-1-16-12288", "S-1-5-21"] {
            let line = format!("token group {wk} enabled");
            assert_eq!(clean(&line), line);
        }
    }

    /// Tester, round 6: the desktop user's SID on a PC joined to Microsoft Entra ID (`S-1-12-1-` and
    /// four numbers, the account's object id), in the Dynamic Lighting line's shape and with the
    /// `_Classes` hive, leaves the log line without its numbers.
    #[test]
    fn entra_id_sid_leaves_the_log_line() {
        let ids = ["3412569011", "1171233452", "2311236254", "1394823476"];
        let out = assert_clean(
            "Dynamic Lighting: reading the desktop user's settings in \
             HKEY_USERS\\S-1-12-1-3412569011-1171233452-2311236254-1394823476",
            &ids,
        );
        assert!(out.ends_with("HKEY_USERS\\S-1-12-1-<account>"), "{out}");
        let out = assert_clean("hive S-1-12-1-3412569011-1171233452-2311236254-1394823476_Classes open", &ids);
        assert!(out.contains("S-1-12-1-<account>_Classes open"), "{out}");
    }

    /// Tester, known limit (docs/SECURITY-AUDIT.md): account identifiers the scrub does not recognise.
    /// A machine or domain SID without its RID (`S-1-5-21-a-b-c`, or with a trailing dash), and a
    /// lower-case SID stay whole. FanRGB writes none of these itself.
    #[test]
    #[ignore = "Known limit (Low, docs/SECURITY-AUDIT.md): RID-less, trailing-dash and lower-case SIDs pass the scrub unchanged"]
    fn sid_shapes_the_scrub_misses() {
        let mut bad = Vec::new();
        for (line, secret) in [
            ("machine SID S-1-5-21-1004336348-1177238915-682003330", "1004336348"),
            ("prefix S-1-5-21-1004336348-1177238915-682003330- end", "1004336348"),
            ("hkey_users\\s-1-5-21-1004336348-1177238915-682003330-1001", "1004336348"),
            ("hkey_users\\s-1-12-1-3412569011-1171233452-2311236254-1394823476", "3412569011"),
        ] {
            if clean(line).contains(secret) {
                bad.push(clean(line));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// Tester: profile paths in the shapes they reach a log line: upper and lower case, `\\?\` long
    /// paths, UNC paths, Debug output with doubled backslashes, a name with spaces, dots or non-ASCII
    /// letters, a path that ends at the profile folder (end of line, before a quote). Shared profiles
    /// (Public, Default, All Users) stay; the rest of the path stays.
    #[test]
    fn profile_paths_lose_the_account_name() {
        let names = ["aoife", "Aoife Byrne", "aoife.byrne", "Éabha Ó Briain", "user_01"];
        for n in names {
            let cases = [
                format!("settings C:\\Users\\{n}\\AppData\\Roaming\\FanRGB\\config.json loaded"),
                format!("settings c:\\users\\{n}\\appdata\\local\\fanrgb"),
                format!("long \\\\?\\C:\\Users\\{n}\\Downloads\\fanrgb.exe"),
                format!("unc \\\\pc\\c$\\Users\\{n}\\Desktop\\x.txt"),
                format!("debug {:?}", format!("C:\\Users\\{n}\\AppData\\Local\\FanRGB")),
                format!("home is C:\\Users\\{n}"),
                format!("quoted 'C:\\Users\\{n}' and \"D:\\Users\\{n}\\x\""),
            ];
            for c in &cases {
                let out = assert_clean(c, &[n]);
                assert!(out.contains("<user>"), "{out}");
            }
        }
        let out = clean("settings C:\\Users\\aoife\\AppData\\Roaming\\FanRGB\\config.json loaded");
        assert_eq!(out, "settings C:\\Users\\<user>\\AppData\\Roaming\\FanRGB\\config.json loaded");
        for shared in ["Public", "Default", "All Users", "public"] {
            let line = format!("C:\\Users\\{shared}\\Documents\\x");
            assert_eq!(clean(&line), line);
        }
    }

    /// Tester, Low: shapes the scrub does not cover. Forward slashes (`C:/Users/name/...`, a
    /// `file:///` URL) keep the name. And a profile folder followed by more text on the line without a
    /// separator swallows that text up to the end of the line (nothing leaks, but the log loses it).
    #[test]
    #[ignore = "Known limit (Low, docs/SECURITY-AUDIT.md): forward-slash profile paths keep the name; (Info) text after a bare profile folder is eaten"]
    fn profile_path_shapes_the_scrub_misses() {
        let mut bad = Vec::new();
        for line in ["config C:/Users/aoife/AppData/Roaming/FanRGB", "url file:///C:/Users/aoife/Desktop/x.html"] {
            if clean(line).contains("aoife") {
                bad.push(format!("leaks: {}", clean(line)));
            }
        }
        let out = clean("profile C:\\Users\\aoife (roaming) has 3 files, 2 errors");
        if !out.contains("has 3 files, 2 errors") {
            bad.push(format!("text lost: {out}"));
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    /// Tester: a token is masked wherever it sits relative to a profile path (inside it, right after
    /// the name, glued to the drive letter, after `Bearer`), and both are gone.
    #[test]
    fn tokens_next_to_paths_are_masked() {
        // Built at run time so secret scanners reading the source don't report a made-up token.
        let tok = format!("ghp_{}", "abcdEFGH1234567890".repeat(2));
        let pat = "github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz";
        for line in [
            format!("C:\\Users\\aoife\\.config\\{tok}"),
            format!("C:\\Users\\{tok}\\AppData"),
            format!("token={tok}C:\\Users\\aoife\\x"),
            format!("Authorization: Bearer {tok} for C:\\Users\\aoife\\x"),
            format!("{pat}\\Users\\aoife\\"),
            format!("\"{pat}\" in {:?}", "C:\\Users\\aoife\\x"),
        ] {
            assert_clean(&line, &["aoife", "abcdEFGH1234567890", "0123456789_abcdefghij"]);
        }
    }

    /// Tester: reproducible random lines built from the pieces that matter (separators, `\Users\`,
    /// SID prefixes, digits, quotes, tokens, multi-byte letters, control characters): `clean` never
    /// panics (byte offsets next to multi-byte text), never lets a line break through, is idempotent
    /// (the report's `tail` cleans lines that `log` already cleaned), and a planted profile name inside
    /// `\Users\<name>\` never survives.
    #[test]
    fn random_lines_never_panic_and_never_keep_a_planted_name() {
        let pieces = [
            "\\Users\\",
            "\\users\\",
            "\\USERS\\",
            "\\",
            "\\\\",
            "/",
            "C:",
            "S-1-5-21-",
            "S-1-12-1-",
            "-",
            "1004336348",
            "42",
            "\"",
            "'",
            "\n",
            "\r",
            "\t",
            "\u{1b}",
            " ",
            "ghp_",
            "github_pat_",
            "Bearer ",
            "abcd1234",
            "Public",
            "_Classes",
            "é",
            "Ó",
            "日本",
            "€",
            "ü\\",
            "\u{200b}",
            "<user>",
            "<machine>",
            "x",
        ];
        let mut seed = 0x0DDB_1A5E_5BAD_5EEDu64;
        let mut rnd = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let mut not_idempotent = 0;
        for k in 0..20_000 {
            let mut s = String::new();
            for _ in 0..rnd(24) + 1 {
                s.push_str(pieces[rnd(pieces.len())]);
            }
            if k % 2 == 0 {
                // A real profile path somewhere in the line.
                let at = (0..=s.len()).filter(|i| s.is_char_boundary(*i)).nth(rnd(s.chars().count() + 1)).unwrap_or(0);
                s.insert_str(at, "C:\\Users\\Zq7Pláinte\\AppData\\");
            }
            let once = clean(&s);
            let twice = clean(&once);
            for out in [&once, &twice] {
                assert!(!out.contains('\n') && !out.contains('\r'), "{s:?} -> {out:?}");
                if k % 2 == 0 {
                    assert!(!out.contains("Zq7Pláinte"), "{s:?} -> {out:?}");
                }
            }
            // The report's `tail` cleans lines `log` already cleaned. Not quite idempotent: a `\Users\`
            // followed straight by a line break keeps an empty name the first time; the second pass then
            // takes the escaped `\n` for a separator and the next text for a name (Info: cosmetic).
            if twice != once {
                not_idempotent += 1;
                assert!(s.to_ascii_lowercase().contains("\\users\\"), "{s:?}: {once:?} -> {twice:?}");
            }
        }
        println!("{not_idempotent} of 20000 lines change on a second clean");
        assert_eq!(clean("C:\\Users\\\nnext"), "C:\\Users\\\\nnext");
        assert_eq!(clean(&clean("C:\\Users\\\nnext")), "C:\\Users\\\\<user>");
    }

    /// Tester: the route itself. A `Logger` writing to a file (and echoing to stderr) puts the cleaned
    /// line in the file and in the in-app recent lines, whatever the record's target.
    #[test]
    fn logger_writes_only_cleaned_lines() {
        let path = std::env::temp_dir().join(format!("fanrgb-logger-tester-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let logger = Logger {
            file: Mutex::new(open_log(&path)),
            path: path.clone(),
            written: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
            level: LevelFilter::Info,
            echo_stderr: true,
        };
        let raw = "Dynamic Lighting: HKEY_USERS\\S-1-5-21-1004336348-1177238915-682003330-1001 \
                   C:\\Users\\aoife\\x ghp_abcdEFGH1234567890\nforged";
        for target in ["fanrgb::rgb::dynlight", "hidapi"] {
            logger.log(&log::Record::builder().args(format_args!("{raw}")).level(Level::Warn).target(target).build());
        }
        logger.flush();
        let text = std::fs::read_to_string(&path).unwrap();
        let recent: Vec<String> = logger.recent.lock().unwrap().iter().cloned().collect();
        let _ = std::fs::remove_file(&path);
        assert_eq!(text.lines().count(), 2, "{text}");
        for l in text.lines().chain(recent.iter().map(String::as_str)) {
            for s in ["1004336348", "aoife", "abcdEFGH1234567890"] {
                assert!(!l.contains(s), "{s} in {l}");
            }
            assert!(
                l.contains("S-1-5-21-<machine>-1001") && l.contains("\\Users\\<user>\\") && l.contains("\\nforged"),
                "{l}"
            );
        }
        assert_eq!(recent.len(), 2);
    }
}
