//! Paths, hashing, time, glob matching and running external commands.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Expands a leading `~` or `~/`.
pub fn expand(path: &str) -> PathBuf {
    if path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// The inverse of `expand`, for display.
pub fn tilde(path: &Path) -> String {
    let h = home();
    match path.strip_prefix(&h) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// A system path. Tests point `DOTFINDER_TEST_ROOT` at a fake file tree.
pub fn sys(path: &str) -> PathBuf {
    match std::env::var_os("DOTFINDER_TEST_ROOT") {
        Some(root) if !root.is_empty() => PathBuf::from(root).join(path.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

pub fn state_dir() -> PathBuf {
    match std::env::var_os("XDG_STATE_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p).join("dotfinder"),
        _ => home().join(".local/state/dotfinder"),
    }
}

/// FNV-1a, 64-bit. Used to notice changes, never for security.
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso8601(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Human-friendly age: "3 days", "5 hours".
pub fn age(secs: u64) -> String {
    let (n, unit) = match secs {
        s if s < 120 => return "just now".to_string(),
        s if s < 7_200 => (s / 60, "minute"),
        s if s < 172_800 => (s / 3600, "hour"),
        s => (s / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// Case-insensitive glob with `*` (any run of characters) and `?` (one).
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

#[derive(Debug)]
pub enum RunError {
    /// The program isn't installed.
    NotFound,
    TimedOut,
    Other(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::NotFound => write!(f, "not installed"),
            RunError::TimedOut => write!(f, "timed out"),
            RunError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Runs a command with captured output, optional stdin text and a timeout.
pub fn run(argv: &[&str], stdin: Option<&str>, timeout: Duration) -> Result<Output, RunError> {
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| RunError::Other("empty command".into()))?;
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1");
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RunError::NotFound,
        _ => RunError::Other(e.to_string()),
    })?;
    if let Some(text) = stdin {
        use std::io::Write;
        let mut pipe = child.stdin.take().unwrap();
        let text = text.to_string();
        // Written from a thread so a large input can't deadlock against a
        // child that is itself blocked writing output.
        std::thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out_pipe.read_to_end(&mut b);
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err_pipe.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(RunError::Other(e.to_string())),
        }
    };
    let stdout = String::from_utf8_lossy(&out_t.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
    Ok(Output {
        status: status.code(),
        stdout,
        stderr,
    })
}

/// `run` with a 60-second timeout and no stdin.
pub fn run_simple(argv: &[&str]) -> Result<Output, RunError> {
    run(argv, None, Duration::from_secs(60))
}

/// Writes a file atomically: a temporary file in the same directory, then a
/// rename. A crash mid-write never leaves a half-written file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.dotfinder-tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        assert!(glob(
            "defaults:*:NSWindow Frame *",
            "defaults:com.apple.finder:NSWindow Frame Main"
        ));
        assert!(glob("*lastused*", "defaults:x:LastUsedDate"));
        assert!(!glob("defaults:com.apple.dock:*", "defaults:com.apple.dockx"));
        assert!(glob("a?c", "abc"));
        assert!(glob("*", ""));
        assert!(!glob("a", ""));
    }

    #[test]
    fn dates() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_792_145_474), "2026-10-16T10:11:14Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn running() {
        let out = run(
            &["sh", "-c", "cat; echo err >&2"],
            Some("hi"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.ok());
        assert_eq!(out.stdout, "hi");
        assert_eq!(out.stderr, "err\n");
        assert!(matches!(
            run_simple(&["definitely-not-a-command-xyz"]),
            Err(RunError::NotFound)
        ));
        assert!(matches!(
            run(&["sleep", "5"], None, Duration::from_millis(100)),
            Err(RunError::TimedOut)
        ));
    }
}
