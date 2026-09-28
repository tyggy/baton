//! `baton setup`, `baton pair`, `baton doctor`: blocking, terminal-facing
//! helpers (no tokio needed — these run before or beside the daemon).

pub mod doctor;
pub mod migrate;
pub mod pairing;
pub mod setup;

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Output of a helper command.
pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run `prog args…` with a timeout; a missing binary or a timeout is
/// `ok: false` with the reason in `stderr`.
pub fn run(prog: &str, args: &[&str], timeout: Duration) -> Out {
    let child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return Out {
                ok: false,
                stdout: String::new(),
                stderr: format!("{prog}: {e}"),
            }
        }
    };
    // Drain the pipes on threads so a chatty child can't block on a full pipe.
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = so.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = se.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    match status {
        Some(s) => Out {
            ok: s.success(),
            stdout,
            stderr,
        },
        None => Out {
            ok: false,
            stdout,
            stderr: format!("{prog}: timed out"),
        },
    }
}

/// `which name`.
pub fn which(name: &str) -> Option<PathBuf> {
    crate::cmd::which(name)
}

/// The first executable among `candidates`, else `name` on PATH.
pub fn find_tool(name: &str, candidates: &[&str]) -> Option<PathBuf> {
    which(name).or_else(|| {
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|p| is_executable(p))
    })
}

pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

pub fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
}

/// Ask on the terminal; empty answer = `default`. Non-interactive = `default`.
pub fn ask(question: &str, default: &str, interactive: bool) -> String {
    if !interactive {
        return default.to_string();
    }
    print!("{question} [{default}]: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return default.to_string();
    }
    let a = line.trim();
    if a.is_empty() {
        default.to_string()
    } else {
        a.to_string()
    }
}

pub fn confirm(question: &str, default: bool, interactive: bool) -> bool {
    let d = if default { "Y/n" } else { "y/N" };
    let a = ask(question, d, interactive);
    match a.to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// The account's real home (from the password database), to tell a
/// sandboxed `HOME` apart from the user's.
pub fn account_home() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer into static storage (or null); we
    // copy the string out immediately.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
        Some(PathBuf::from(dir.to_string_lossy().into_owned()))
    }
}

/// Write `text` to `path` (creating parents) only when it differs.
/// Returns whether the file changed.
pub fn write_if_changed(path: &Path, text: &str, mode: Option<u32>) -> std::io::Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        if let Some(m) = mode {
            set_mode(path, m)?;
        }
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp-baton");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        if let Some(m) = mode {
            set_mode(&tmp, m)?;
        }
        f.write_all(text.as_bytes())?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// A GET to the local daemon (plain HTTP/1.0, no dependencies): `(status, body)`.
pub fn http_get(base: &str, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let hostport = base
        .strip_prefix("http://")
        .ok_or_else(|| format!("not an http:// URL: {base}"))?
        .trim_end_matches('/');
    let addr = hostport
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no address")?;
    let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    write!(
        s,
        "GET {path} HTTP/1.0\r\nHost: {hostport}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or("bad HTTP response")?;
    Ok((status, body.to_string()))
}

/// Last `n` lines of a file (reads at most the final 1 MB).
pub fn tail_lines(path: &Path, n: usize) -> Vec<String> {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return vec![];
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(1 << 20);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // partial first line
    }
    let skip = lines.len().saturating_sub(n);
    lines.split_off(skip)
}
