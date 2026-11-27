//! What the rest needs of the system: the home folder, a log time, dates,
//! and running a program with a deadline.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The user's home folder: `%USERPROFILE%` on Windows, `$HOME` elsewhere.
pub fn home() -> Option<PathBuf> {
    let var = if cfg!(target_os = "windows") { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// Now as the log writes it, in UTC: "2026-11-20 15:00:00Z".
pub fn stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}Z", s / 3600, s % 3600 / 60, s % 60)
}

/// Days since 1970-01-01 to a date: year, month, day (Howard Hinnant's
/// civil_from_days).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// The inverse: a date to days since 1970-01-01.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Keep a console program run in the background from flashing up a window
/// on Windows (CREATE_NO_WINDOW). Nothing to do elsewhere.
pub fn no_window(cmd: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// What a program run with `output` said.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
}

/// Run `cmd` to its end, or until `deadline` (then it's killed): its exit
/// and what it printed. stderr is left out.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn output(cmd: &mut Command, deadline: Duration) -> Result<Output, String> {
    let mut child = no_window(cmd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
    // Read on a thread of its own, so a full pipe never holds the child up.
    let pipe = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut bytes);
        }
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    Ok(Output { status, stdout })
}

/// Run `cmd` with `input` on its stdin, to its end or until `deadline` (then
/// it's killed): its exit and what it said on stderr. stdout is dropped.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn run_with_input(cmd: &mut Command, input: &[u8], deadline: Duration) -> Result<(ExitStatus, String), String> {
    use std::io::Write;
    let mut child = no_window(cmd).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    if let Some(mut i) = child.stdin.take() {
        let _ = i.write_all(input);
    }
    let pipe = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut bytes);
        }
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok((status, rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()))
}

/// A folder of this tool's (its store, its cache) made, and on Unix readable
/// by you only: account names, what sessions work on.
pub fn private_dir(p: &Path) {
    let _ = std::fs::create_dir_all(p);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(p) {
            if m.permissions().mode() & 0o077 != 0 {
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
}

/// A path as people write it: Windows' `\\?\C:\...` (what canonicalize gives
/// there) as `C:\...`. Others as they are.
pub fn plain_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.get(1..2) == Some(":") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// This PC's time zone now, in seconds east of UTC.
pub fn utc_offset_secs() -> i64 {
    i64::from(chrono::Local::now().offset().local_minus_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_go_both_ways() {
        for days in [-1, 0, 1, 365, 10_957, 20_412, 1_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn windows_long_paths_read_plainly() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\Users\me\claude-accounts.exe")), PathBuf::from(r"C:\Users\me\claude-accounts.exe"));
        assert_eq!(plain_path(Path::new(r"\\?\UNC\server\share\x")), PathBuf::from(r"\\?\UNC\server\share\x"));
        assert_eq!(plain_path(Path::new("/usr/local/bin/claude-accounts")), PathBuf::from("/usr/local/bin/claude-accounts"));
    }

    #[test]
    fn a_log_time_is_utc() {
        let s = stamp();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z'));
    }
}
