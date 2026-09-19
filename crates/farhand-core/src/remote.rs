//! Everything that touches the remote host: one multiplexed OpenSSH
//! session, an SFTP channel on top of it, and the operations the tools are
//! built from.
//!
//! Design rules:
//! - Authentication is OpenSSH's job. FarHand starts `ssh <host>` and lets
//!   the user's own config supply keys, agents, jump hosts and ports.
//! - Command text is never shell-escaped by hand. It is base64-encoded and
//!   decoded on the remote, so quoting cannot go wrong.
//! - The local environment is never forwarded.
//! - Every command runs under the remote `timeout(1)` with a local watchdog
//!   behind it.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use bytes::BytesMut;
use openssh::{KnownHosts, Session, SessionBuilder, Stdio};
use openssh_sftp_client::{Sftp, SftpOptions};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Semaphore};

use crate::config::{Limits, RemoteConfig};
use crate::error::{Error, Result};
use crate::text::shell_quote;

/// Remote `timeout(1)` exit statuses.
const TIMEOUT_EXIT: i32 = 124;
/// Extra seconds the local watchdog waits past the remote timeout.
const WATCHDOG_GRACE: u64 = 15;
/// Exit status the wrapper uses when `cd` into the working directory fails.
const CD_FAILED_EXIT: i32 = 97;
/// Concurrent channels on the one session. sshd's default `MaxSessions` is
/// 10 and the SFTP channel holds one permanently; agents that fan out tool
/// calls otherwise hit "failed to connect to the ssh multiplex server".
const MAX_CHANNELS: usize = 4;

/// The remote as reachable from the local machine.
#[derive(Debug, Clone)]
pub struct Address {
    pub hostname: String,
    pub port: u16,
    /// The destination as written in the config (an alias or `user@host`).
    pub alias: String,
}

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub duration: Duration,
}

struct Conn {
    session: Arc<Session>,
    sftp: Sftp,
    home: String,
    /// The configured workdir with `~` expanded to `home`.
    workdir: String,
}

pub struct Remote {
    cfg: RemoteConfig,
    limits: Limits,
    conn: Mutex<Option<Arc<Conn>>>,
    channels: Semaphore,
}

impl Remote {
    pub fn new(cfg: RemoteConfig, limits: Limits) -> Self {
        Self {
            cfg,
            limits,
            conn: Mutex::new(None),
            channels: Semaphore::new(MAX_CHANNELS),
        }
    }

    pub fn host(&self) -> &str {
        &self.cfg.host
    }

    /// How the remote is addressed from this machine, as OpenSSH resolves
    /// the destination: `(hostname, port)`. The model needs this to report
    /// URLs the user can open; an alias from `~/.ssh/config` means nothing
    /// to a browser. Falls back to the configured destination.
    pub fn address(&self) -> Address {
        let fallback = Address {
            hostname: self
                .cfg
                .host
                .rsplit('@')
                .next()
                .unwrap_or(&self.cfg.host)
                .to_string(),
            port: 22,
            alias: self.cfg.host.clone(),
        };
        let out = match std::process::Command::new("ssh")
            .arg("-G")
            .arg(&self.cfg.host)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Ok(o) if o.status.success() => o.stdout,
            _ => return fallback,
        };
        let text = String::from_utf8_lossy(&out);
        let mut addr = fallback;
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("hostname", v)) => addr.hostname = v.trim().to_string(),
                Some(("port", v)) => addr.port = v.trim().parse().unwrap_or(22),
                _ => {}
            }
        }
        addr
    }

    /// The workdir as configured (`~` not yet expanded).
    pub fn workdir(&self) -> &str {
        &self.cfg.workdir
    }

    /// The workdir with `~` expanded; connects if needed.
    pub async fn effective_workdir(&self) -> Result<String> {
        Ok(self.conn().await?.workdir.clone())
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Connect now, or report why that is impossible.
    pub async fn connect(&self) -> Result<()> {
        self.conn().await.map(|_| ())
    }

    pub async fn is_connected(&self) -> bool {
        match &*self.conn.lock().await {
            Some(c) => c.session.check().await.is_ok(),
            None => false,
        }
    }

    async fn conn(&self) -> Result<Arc<Conn>> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = &*slot {
            if c.session.check().await.is_ok() {
                return Ok(c.clone());
            }
            tracing::warn!("ssh session to {} dropped; reconnecting", self.cfg.host);
            *slot = None;
        }
        // Strict host-key checking: FarHand only talks to hosts the user has
        // already connected to by hand. A `.farhand.toml` that arrives with a
        // cloned repository therefore cannot point an agent at a machine the
        // user never chose.
        let mut builder = SessionBuilder::default();
        builder
            .connect_timeout(Duration::from_secs(self.cfg.connect_timeout_secs))
            .server_alive_interval(Duration::from_secs(self.cfg.server_alive_interval_secs))
            .known_hosts_check(KnownHosts::Strict);
        let session = builder.connect_mux(&self.cfg.host).await.map_err(|e| {
            Error::Ssh(format!(
                "cannot connect to `{}`: {e}. Check that `ssh {}` works from a terminal; \
                 FarHand refuses hosts whose key is not yet in known_hosts, so connect once \
                 by hand first.",
                self.cfg.host, self.cfg.host
            ))
        })?;
        let session = Arc::new(session);
        let sftp = Sftp::from_clonable_session(session.clone(), SftpOptions::default())
            .await
            .map_err(|e| {
                Error::Ssh(format!(
                    "sftp subsystem unavailable on `{}`: {e}",
                    self.cfg.host
                ))
            })?;
        let home = {
            let out = session
                .command("sh")
                .arg("-c")
                .arg("printf %s \"$HOME\"")
                .output()
                .await?;
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let workdir = expand_tilde(&self.cfg.workdir, &home);
        let conn = Arc::new(Conn {
            session,
            sftp,
            home,
            workdir,
        });
        *slot = Some(conn.clone());
        tracing::info!(host = %self.cfg.host, "connected");
        Ok(conn)
    }

    /// Absolute remote path for what the model typed: `~` expands to the
    /// remote home, relative paths hang off the working directory.
    pub async fn resolve(&self, raw: &str) -> Result<String> {
        let raw = raw.trim();
        if raw.starts_with('/') {
            return Ok(normalize(raw));
        }
        let conn = self.conn().await?;
        if raw.is_empty() {
            return Ok(conn.workdir.clone());
        }
        if raw == "~" || raw.starts_with("~/") {
            return Ok(normalize(&expand_tilde(raw, &conn.home)));
        }
        Ok(normalize(&format!("{}/{raw}", conn.workdir)))
    }

    /// Run `command` through the configured shell in `cwd` (default: the
    /// working directory). `timeout_secs` is clamped to the configured
    /// maximum.
    pub async fn exec(
        &self,
        command: &str,
        cwd: Option<&str>,
        timeout_secs: Option<u64>,
    ) -> Result<ExecOutput> {
        let conn = self.conn().await?;
        let cwd = match cwd {
            Some(c) => self.resolve(c).await?,
            None => conn.workdir.clone(),
        };
        let timeout = timeout_secs
            .unwrap_or(self.limits.command_timeout_secs)
            .clamp(1, self.limits.max_command_timeout_secs);

        // The script the user's shell runs. Colour is off because the model
        // reads the output; nothing else about the remote environment is
        // touched, and nothing local is forwarded.
        let script = format!(
            "cd {} || exit {CD_FAILED_EXIT}\nexport NO_COLOR=1\n{command}\n",
            shell_quote(&cwd)
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(script.as_bytes());
        let shell = self
            .cfg
            .shell
            .iter()
            .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" ");
        // Outer `sh -c` so the remote login shell only has to parse one
        // single-quoted word; the inner script has no single quotes because
        // base64 has none and the shell argv is double-quoted.
        let inner = format!("timeout -k 5 {timeout} {shell} \"$(printf %s {b64} | base64 -d)\"");

        let _permit = self.channels.acquire().await.expect("semaphore open");
        let started = Instant::now();
        let mut cmd = conn.session.raw_command("sh");
        cmd.raw_arg("-c")
            .raw_arg(format!("'{inner}'"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().await?;
        let stdout = child.stdout().take().expect("piped stdout");
        let stderr = child.stderr().take().expect("piped stderr");
        let cap = self.limits.max_output_bytes;

        let collect = async {
            let (out, err) =
                tokio::join!(collect_bounded(stdout, cap), collect_bounded(stderr, cap));
            let status = child.wait().await?;
            Ok::<_, Error>((out?, err?, status))
        };
        let watchdog = Duration::from_secs(timeout + WATCHDOG_GRACE);
        let (out, err, status) = match tokio::time::timeout(watchdog, collect).await {
            Ok(r) => r?,
            Err(_) => {
                tracing::warn!("local watchdog fired after {}s", watchdog.as_secs());
                return Err(Error::Timeout { secs: timeout });
            }
        };
        let exit_code = status.code().unwrap_or(-1);
        let timed_out = exit_code == TIMEOUT_EXIT;
        if exit_code == CD_FAILED_EXIT && out.data.is_empty() {
            return Err(Error::Invalid(format!(
                "cannot cd into `{cwd}` on the remote"
            )));
        }
        Ok(ExecOutput {
            exit_code,
            stdout: out.data,
            stderr: err.data,
            stdout_truncated: out.truncated,
            stderr_truncated: err.truncated,
            timed_out,
            duration: started.elapsed(),
        })
    }

    /// Read at most `max` bytes of a remote file. Returns the bytes and the
    /// file's full length.
    pub async fn read_file(&self, path: &str, max: usize) -> Result<(Vec<u8>, u64)> {
        let conn = self.conn().await?;
        let path = self.resolve(path).await?;
        let _permit = self.channels.acquire().await.expect("semaphore open");
        let mut fs = conn.sftp.fs();
        let meta = fs.metadata(&path).await.map_err(|e| not_found(&path, e))?;
        if meta.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            return Err(Error::Invalid(format!("`{path}` is a directory")));
        }
        let len = meta.len().unwrap_or(0);
        let mut file = conn
            .sftp
            .open(&path)
            .await
            .map_err(|e| not_found(&path, e))?;
        let want = (len as usize).min(max);
        let mut buf = BytesMut::with_capacity(want);
        let mut remaining = want;
        while remaining > 0 {
            let chunk = remaining.min(256 * 1024) as u32;
            match file.read(chunk, buf.split_off(buf.len())).await? {
                Some(bytes) => {
                    remaining -= bytes.len();
                    buf.unsplit(bytes);
                }
                None => break,
            }
        }
        let _ = file.close().await;
        Ok((buf.to_vec(), len))
    }

    /// Write a whole file, creating parent directories.
    pub async fn write_file(&self, path: &str, content: &[u8]) -> Result<String> {
        let conn = self.conn().await?;
        let path = self.resolve(path).await?;
        if let Some(parent) = Path::new(&path).parent() {
            let mkdir = format!("mkdir -p {}", shell_quote(&parent.to_string_lossy()));
            let out = self.exec(&mkdir, None, Some(30)).await?;
            if out.exit_code != 0 {
                return Err(Error::Ssh(format!(
                    "mkdir -p failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        let _permit = self.channels.acquire().await.expect("semaphore open");
        conn.sftp.fs().write(&path, content).await?;
        Ok(path)
    }

    pub async fn exists(&self, path: &str) -> Result<Option<bool>> {
        let conn = self.conn().await?;
        let path = self.resolve(path).await?;
        let _permit = self.channels.acquire().await.expect("semaphore open");
        match conn.sftp.fs().metadata(&path).await {
            Ok(m) => Ok(Some(m.file_type().map(|t| t.is_dir()).unwrap_or(false))),
            Err(_) => Ok(None),
        }
    }

    /// Whether a remote executable is on PATH.
    pub async fn has_tool(&self, name: &str) -> Result<bool> {
        let out = self
            .exec(
                &format!("command -v {} >/dev/null 2>&1", shell_quote(name)),
                None,
                Some(15),
            )
            .await?;
        Ok(out.exit_code == 0)
    }
}

fn not_found(path: &str, e: openssh_sftp_client::Error) -> Error {
    Error::Ssh(format!("`{path}`: {e}"))
}

/// `~` and `~/x` become `home` and `home/x`; anything else is unchanged.
fn expand_tilde(p: &str, home: &str) -> String {
    if p == "~" {
        home.to_string()
    } else if let Some(rest) = p.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        p.to_string()
    }
}

/// Collapse `.` and `..` segments without touching the filesystem.
pub fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

struct Collected {
    data: Vec<u8>,
    truncated: bool,
}

/// Read a stream to its end, keeping at most the first two thirds and the
/// last third of `cap` bytes so a runaway command cannot exhaust memory.
async fn collect_bounded<R: AsyncReadExt + Unpin>(mut r: R, cap: usize) -> Result<Collected> {
    let head_cap = cap * 2 / 3;
    let tail_cap = cap - head_cap;
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let mut truncated = false;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let mut chunk = &buf[..n];
        if head.len() < head_cap {
            let take = chunk.len().min(head_cap - head.len());
            head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        if !chunk.is_empty() {
            truncated = true;
            tail.extend_from_slice(chunk);
            if tail.len() > tail_cap * 2 {
                let drop = tail.len() - tail_cap;
                tail.drain(..drop);
            }
        }
    }
    if truncated {
        if tail.len() > tail_cap {
            let drop = tail.len() - tail_cap;
            tail.drain(..drop);
        }
        let omitted_note = format!("\n\n[... output truncated; limit is {cap} bytes ...]\n\n");
        head.extend_from_slice(omitted_note.as_bytes());
        head.extend_from_slice(&tail);
    }
    Ok(Collected {
        data: head,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        assert_eq!(expand_tilde("~", "/root"), "/root");
        assert_eq!(expand_tilde("~/a/b", "/root"), "/root/a/b");
        assert_eq!(expand_tilde("/x", "/root"), "/x");
        assert_eq!(expand_tilde("rel", "/root"), "rel");
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(normalize("/../x"), "/x");
        assert_eq!(normalize("/"), "/");
    }

    #[tokio::test]
    async fn bounded_collection_keeps_head_and_tail() {
        let data: Vec<u8> = (0..10_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let c = collect_bounded(&data[..], 300).await.unwrap();
        assert!(c.truncated);
        assert!(c.data.starts_with(b"abcdefghij"));
        assert!(c.data.ends_with(&data[data.len() - 100..]));
        let c = collect_bounded(&b"tiny"[..], 300).await.unwrap();
        assert!(!c.truncated);
        assert_eq!(c.data, b"tiny");
    }
}
