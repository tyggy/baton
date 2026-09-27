//! Subprocess helpers with the Python server's semantics (`run_cmd`,
//! fire-and-forget `subprocess.Popen`). Commands resolve through `PATH`,
//! like Python's, so stand-ins placed first on `PATH` are honoured.
#![allow(dead_code)] // API for the other groups

use std::process::Stdio;
use std::time::Duration;

/// `(stdout, stderr, returncode)` of `run_cmd`.
pub struct Out {
    pub stdout: String,
    pub stderr: String,
    pub rc: i32,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.rc == 0
    }
}

/// `run_cmd(args, timeout)`: capture output; a missing binary is
/// `("", "command not found: <arg0>", 1)` and a timeout `("", "timeout", 1)`.
pub async fn run(args: &[&str], timeout_s: f64) -> Out {
    let mut c = tokio::process::Command::new(args[0]);
    c.args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Out {
                stdout: String::new(),
                stderr: format!("command not found: {}", args[0]),
                rc: 1,
            }
        }
        Err(e) => {
            return Out {
                stdout: String::new(),
                stderr: e.to_string(),
                rc: 1,
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs_f64(timeout_s), child.wait_with_output()).await {
        Ok(Ok(o)) => Out {
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            rc: o.status.code().unwrap_or(-1),
        },
        Ok(Err(e)) => Out {
            stdout: String::new(),
            stderr: e.to_string(),
            rc: 1,
        },
        Err(_) => Out {
            stdout: String::new(),
            stderr: "timeout".into(),
            rc: 1,
        },
    }
}

/// `subprocess.run([...]).returncode` without capture (stdout/stderr
/// discarded), `None` when the binary is missing (Python raises there).
pub async fn status(args: &[&str]) -> Option<i32> {
    tokio::process::Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .ok()
        .map(|s| s.code().unwrap_or(-1))
}

/// `subprocess.Popen(args)`: start and forget (reaped in the background).
/// Output goes to the daemon's own stdout/stderr, as Python's does.
pub fn popen(args: &[&str]) {
    let spawned = tokio::process::Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

/// Look `name` up on `PATH` (`which`).
pub fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_semantics() {
        let o = run(&["sh", "-c", "printf hi; printf err >&2; exit 3"], 5.0).await;
        assert_eq!(
            (o.stdout.as_str(), o.stderr.as_str(), o.rc),
            ("hi", "err", 3)
        );
        let o = run(&["definitely-not-a-binary-xyz"], 5.0).await;
        assert_eq!(o.stderr, "command not found: definitely-not-a-binary-xyz");
        let o = run(&["sleep", "5"], 0.2).await;
        assert_eq!((o.stderr.as_str(), o.rc), ("timeout", 1));
    }
}
