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
//! - Every command runs under a remote timeout with a local watchdog
//!   behind it.
//! - Directory creation and tree walking go through SFTP, not the shell,
//!   so they behave the same on every platform.
//!
//! Two platforms: POSIX (a `sh`, `timeout(1)` and `base64(1)`; the
//! configured shell runs the script) and Windows with OpenSSH Server, where
//! the script runs in PowerShell. See [`Platform`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use bytes::BytesMut;
use futures_util::StreamExt;
use openssh::{KnownHosts, Session, SessionBuilder, Stdio};
use openssh_sftp_client::{Sftp, SftpOptions};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Semaphore};

use crate::config::{default_shell, Limits, Os, RemoteConfig};
use crate::error::{Error, Result};
use crate::text::shell_quote;

/// Remote `timeout(1)` exit status; the Windows wrapper uses the same.
const TIMEOUT_EXIT: i32 = 124;
/// Extra seconds the local watchdog waits past the remote timeout.
const WATCHDOG_GRACE: u64 = 15;
/// Exit status the wrapper uses when `cd` into the working directory fails.
const CD_FAILED_EXIT: i32 = 97;
/// Concurrent channels on the one session. sshd's default `MaxSessions` is
/// 10 and the SFTP channel holds one permanently; agents that fan out tool
/// calls otherwise hit "failed to connect to the ssh multiplex server".
const MAX_CHANNELS: usize = 4;
/// Last line the Windows wrapper prints so the exit status of a script
/// that ended normally survives PowerShell's own 0/1 convention.
const WIN_EXIT_MARKER: &str = "__FARHAND_EXIT__=";

/// What the remote runs, detected from the SFTP root on connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Posix,
    Windows,
}

impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Platform::Posix => "posix",
            Platform::Windows => "windows",
        }
    }
}

/// On Windows, what sshd hands our command line to (its `DefaultShell`).
/// The exit status of a nested `powershell` is only propagated by `cmd`;
/// under PowerShell we have to ask for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginShell {
    PowerShell,
    Cmd,
}

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
    platform: Platform,
    login_shell: LoginShell,
    /// Native form: `/home/me` or `C:\Users\me`.
    home: String,
    /// The configured workdir with `~` expanded to `home`, native form.
    workdir: String,
}

pub struct Remote {
    cfg: RemoteConfig,
    limits: Limits,
    conn: Mutex<Option<Arc<Conn>>>,
    channels: Semaphore,
    /// Directories known to exist on the remote (native form), so a tree
    /// upload does not re-check every parent for every file.
    known_dirs: Mutex<HashSet<String>>,
}

impl Remote {
    pub fn new(cfg: RemoteConfig, limits: Limits) -> Self {
        Self {
            cfg,
            limits,
            conn: Mutex::new(None),
            channels: Semaphore::new(MAX_CHANNELS),
            known_dirs: Mutex::new(HashSet::new()),
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

    /// The platform as configured, before any connection: `None` for
    /// `auto`.
    pub fn configured_platform(&self) -> Option<Platform> {
        match self.cfg.os {
            Os::Auto => None,
            Os::Posix => Some(Platform::Posix),
            Os::Windows => Some(Platform::Windows),
        }
    }

    /// The detected platform; connects if needed.
    pub async fn platform(&self) -> Result<Platform> {
        Ok(self.conn().await?.platform)
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
            self.known_dirs.lock().await.clear();
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
        // The SFTP session starts in the user's home. Its canonical form
        // also tells the platform apart: Windows OpenSSH reports `/C:/...`.
        let sftp_home = sftp
            .fs()
            .canonicalize(".")
            .await
            .map_err(|e| Error::Ssh(format!("cannot resolve the remote home: {e}")))?
            .to_string_lossy()
            .into_owned();
        let detected = if is_sftp_drive_path(&sftp_home) {
            Platform::Windows
        } else {
            Platform::Posix
        };
        let platform = match (self.cfg.os, detected) {
            (Os::Auto, d) => d,
            (Os::Posix, Platform::Posix) | (Os::Windows, Platform::Windows) => detected,
            (configured, d) => {
                tracing::warn!(
                    "config says os = {:?} but the remote looks like {}; using {}",
                    configured,
                    d.name(),
                    d.name()
                );
                d
            }
        };
        let home = match platform {
            Platform::Posix => sftp_home,
            Platform::Windows => from_sftp_path(&sftp_home),
        };
        let login_shell = match platform {
            Platform::Posix => LoginShell::PowerShell, // unused
            Platform::Windows => {
                // `%OS%` is expanded by cmd and left alone by PowerShell.
                let out = session
                    .raw_command("echo %OS%")
                    .stdin(Stdio::null())
                    .output()
                    .await?;
                if String::from_utf8_lossy(&out.stdout).contains("Windows_NT") {
                    LoginShell::Cmd
                } else {
                    LoginShell::PowerShell
                }
            }
        };
        let workdir = normalize_for(platform, &expand_tilde(&self.cfg.workdir, &home));
        let conn = Arc::new(Conn {
            session,
            sftp,
            platform,
            login_shell,
            home,
            workdir,
        });
        *slot = Some(conn.clone());
        tracing::info!(host = %self.cfg.host, platform = platform.name(), "connected");
        Ok(conn)
    }

    /// Absolute remote path, in the remote's native form, for what the
    /// model typed: `~` expands to the remote home, relative paths hang off
    /// the working directory. `/` and `\` are both accepted on Windows.
    pub async fn resolve(&self, raw: &str) -> Result<String> {
        let raw = raw.trim();
        let conn = self.conn().await?;
        if raw.is_empty() {
            return Ok(conn.workdir.clone());
        }
        if is_absolute_for(conn.platform, raw) {
            return Ok(normalize_for(conn.platform, raw));
        }
        if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
            return Ok(normalize_for(conn.platform, &expand_tilde(raw, &conn.home)));
        }
        Ok(normalize_for(
            conn.platform,
            &format!("{}/{raw}", conn.workdir),
        ))
    }

    /// Run `command` in `cwd` (default: the working directory) through the
    /// configured shell — PowerShell on Windows. `timeout_secs` is clamped
    /// to the configured maximum.
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

        let _permit = self.channels.acquire().await.expect("semaphore open");
        let started = Instant::now();
        let mut cmd = match conn.platform {
            Platform::Posix => {
                // The script the user's shell runs. Colour is off because
                // the model reads the output; nothing else about the remote
                // environment is touched, and nothing local is forwarded.
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
                // Outer `sh -c` so the remote login shell only has to parse
                // one single-quoted word; the inner script has no single
                // quotes because base64 has none and the argv is
                // double-quoted.
                let inner =
                    format!("timeout -k 5 {timeout} {shell} \"$(printf %s {b64} | base64 -d)\"");
                let mut c = conn.session.raw_command("sh");
                c.raw_arg("-c").raw_arg(format!("'{inner}'"));
                c
            }
            Platform::Windows => {
                let (exe, extra) = self.powershell();
                let outer = windows_wrapper(exe, &extra, &cwd, command, timeout);
                let mut c = conn.session.raw_command(exe);
                for a in &extra {
                    c.raw_arg(a);
                }
                c.raw_arg("-NoProfile")
                    .raw_arg("-NonInteractive")
                    .raw_arg("-ExecutionPolicy")
                    .raw_arg("Bypass")
                    .raw_arg("-EncodedCommand")
                    .raw_arg(encode_utf16_b64(&outer));
                if conn.login_shell == LoginShell::PowerShell {
                    // `powershell -c "<cmd>"` would otherwise turn any
                    // non-zero status into 1.
                    c.raw_arg("; exit $LASTEXITCODE");
                }
                c
            }
        };
        cmd.stdin(Stdio::null())
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
        let (mut out, mut err, status) = match tokio::time::timeout(watchdog, collect).await {
            Ok(r) => r?,
            Err(_) => {
                tracing::warn!("local watchdog fired after {}s", watchdog.as_secs());
                return Err(Error::Timeout { secs: timeout });
            }
        };
        let mut exit_code = status.code().unwrap_or(-1);
        if conn.platform == Platform::Windows {
            if let Some(code) = take_windows_exit_marker(&mut out.data) {
                exit_code = code;
            }
            out.data = crlf_to_lf(&out.data);
            err.data = crlf_to_lf(&decode_clixml(&err.data));
        }
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

    /// The PowerShell executable and any extra leading arguments, from
    /// `remote.shell` unless that is still the POSIX default.
    fn powershell(&self) -> (&str, Vec<String>) {
        if self.cfg.shell == default_shell() || self.cfg.shell.is_empty() {
            ("powershell.exe", Vec::new())
        } else {
            (&self.cfg.shell[0], self.cfg.shell[1..].to_vec())
        }
    }

    /// Read at most `max` bytes of a remote file. Returns the bytes and the
    /// file's full length.
    pub async fn read_file(&self, path: &str, max: usize) -> Result<(Vec<u8>, u64)> {
        let conn = self.conn().await?;
        let path = self.resolve(path).await?;
        let sftp_path = to_sftp_path(conn.platform, &path);
        let _permit = self.channels.acquire().await.expect("semaphore open");
        let mut fs = conn.sftp.fs();
        let meta = fs
            .metadata(&sftp_path)
            .await
            .map_err(|e| not_found(&path, e))?;
        if meta.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            return Err(Error::Invalid(format!("`{path}` is a directory")));
        }
        let len = meta.len().unwrap_or(0);
        let mut file = conn
            .sftp
            .open(&sftp_path)
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
        if let Some(parent) = parent_of(conn.platform, &path) {
            self.ensure_dir(&parent).await?;
        }
        let _permit = self.channels.acquire().await.expect("semaphore open");
        conn.sftp
            .fs()
            .write(to_sftp_path(conn.platform, &path), content)
            .await
            .map_err(|e| Error::Ssh(format!("cannot write `{path}`: {e}")))?;
        Ok(path)
    }

    /// Create `dir` and any missing parents over SFTP. `dir` is a resolved
    /// native path.
    pub async fn ensure_dir(&self, dir: &str) -> Result<()> {
        let conn = self.conn().await?;
        if self.known_dirs.lock().await.contains(dir) {
            return Ok(());
        }
        // Walk up to the nearest existing ancestor, then create downwards.
        let mut missing: Vec<String> = Vec::new();
        let mut cur = dir.to_string();
        loop {
            if self.known_dirs.lock().await.contains(&cur) {
                break;
            }
            let _permit = self.channels.acquire().await.expect("semaphore open");
            match conn
                .sftp
                .fs()
                .metadata(to_sftp_path(conn.platform, &cur))
                .await
            {
                Ok(m) => {
                    if !m.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        return Err(Error::Invalid(format!(
                            "`{cur}` exists on the remote and is not a directory"
                        )));
                    }
                    self.known_dirs.lock().await.insert(cur.clone());
                    break;
                }
                Err(_) => {
                    missing.push(cur.clone());
                    match parent_of(conn.platform, &cur) {
                        Some(p) => cur = p,
                        None => break,
                    }
                }
            }
        }
        for d in missing.into_iter().rev() {
            let _permit = self.channels.acquire().await.expect("semaphore open");
            if let Err(e) = conn
                .sftp
                .fs()
                .create_dir(to_sftp_path(conn.platform, &d))
                .await
            {
                // Another call may have created it meanwhile.
                let exists = conn
                    .sftp
                    .fs()
                    .metadata(to_sftp_path(conn.platform, &d))
                    .await
                    .map(|m| m.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .unwrap_or(false);
                if !exists {
                    return Err(Error::Ssh(format!("cannot create directory `{d}`: {e}")));
                }
            }
            self.known_dirs.lock().await.insert(d);
        }
        Ok(())
    }

    pub async fn exists(&self, path: &str) -> Result<Option<bool>> {
        let conn = self.conn().await?;
        let path = self.resolve(path).await?;
        let _permit = self.channels.acquire().await.expect("semaphore open");
        match conn
            .sftp
            .fs()
            .metadata(to_sftp_path(conn.platform, &path))
            .await
        {
            Ok(m) => Ok(Some(m.file_type().map(|t| t.is_dir()).unwrap_or(false))),
            Err(_) => Ok(None),
        }
    }

    /// Every regular file under `dir` (a resolved native path), as native
    /// paths, over SFTP. Symlinks are not followed. Stops after `limit`
    /// entries so a huge tree is refused quickly rather than listed.
    pub async fn walk_files(&self, dir: &str, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn().await?;
        let sep = separator(conn.platform);
        let mut files = Vec::new();
        let mut stack = vec![dir.to_string()];
        while let Some(d) = stack.pop() {
            let _permit = self.channels.acquire().await.expect("semaphore open");
            let handle = conn
                .sftp
                .fs()
                .open_dir(to_sftp_path(conn.platform, &d))
                .await
                .map_err(|e| Error::Ssh(format!("cannot list `{d}`: {e}")))?;
            let mut entries = std::pin::pin!(handle.read_dir());
            while let Some(entry) = entries.next().await {
                let entry = entry.map_err(|e| Error::Ssh(format!("cannot list `{d}`: {e}")))?;
                let name = entry.filename().to_string_lossy().into_owned();
                if name == "." || name == ".." {
                    continue;
                }
                let full = format!("{d}{sep}{name}");
                match entry.file_type() {
                    Some(t) if t.is_dir() => stack.push(full),
                    Some(t) if t.is_file() => {
                        files.push(full);
                        if files.len() > limit {
                            return Ok(files);
                        }
                    }
                    _ => {}
                }
            }
        }
        files.sort();
        Ok(files)
    }

    /// Whether a remote executable is on PATH.
    pub async fn has_tool(&self, name: &str) -> Result<bool> {
        let conn = self.conn().await?;
        let cmd = match conn.platform {
            Platform::Posix => format!("command -v {} >/dev/null 2>&1", shell_quote(name)),
            Platform::Windows => format!(
                "if (Get-Command {} -ErrorAction SilentlyContinue) {{ 'yes' }} else {{ 'no' }}",
                ps_quote(name)
            ),
        };
        let out = self.exec(&cmd, None, Some(15)).await?;
        Ok(match conn.platform {
            Platform::Posix => out.exit_code == 0,
            Platform::Windows => String::from_utf8_lossy(&out.stdout).contains("yes"),
        })
    }
}

fn not_found(path: &str, e: openssh_sftp_client::Error) -> Error {
    Error::Ssh(format!("`{path}`: {e}"))
}

// ---- Windows command wrapper ------------------------------------------

/// Quote for PowerShell as a single-quoted literal.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// UTF-16LE base64, what `powershell -EncodedCommand` expects.
fn encode_utf16_b64(script: &str) -> String {
    let bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The PowerShell script that runs the model's command on Windows.
///
/// Two processes: the outer one enforces the timeout (`Start-Process`,
/// `WaitForExit`, `taskkill /T`), the inner one runs the command with the
/// working directory set, UTF-8 output and colour off. Output flows through
/// inherited handles, so nothing is buffered in files.
///
/// The inner script must end naturally: `exit` from a `-EncodedCommand`
/// script discards output still in PowerShell's formatting pipeline. So the
/// last thing it prints is [`WIN_EXIT_MARKER`] with the status, which
/// [`take_windows_exit_marker`] reads back. The status follows shell
/// convention: that of the last statement (`$?`), with `$LASTEXITCODE` for
/// a failed native command. `$Error` is deliberately not consulted: it also
/// collects every stderr line a native program wrote and every error that
/// `-ErrorAction SilentlyContinue` hid. A command that calls `exit` itself
/// skips the marker and its status arrives as the process status instead.
fn windows_wrapper(exe: &str, extra: &[String], cwd: &str, command: &str, timeout: u64) -> String {
    let inner = format!(
        "$ProgressPreference='SilentlyContinue'; $ErrorActionPreference='Continue'\n\
         [Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8\n\
         $env:NO_COLOR='1'\n\
         try {{ Set-Location -LiteralPath {cwd} -ErrorAction Stop }} \
         catch {{ [Console]::Error.WriteLine('cannot cd'); exit {CD_FAILED_EXIT} }}\n\
         $global:LASTEXITCODE=0\n\
         {command}\n\
         $__fh_ok=$?\n\
         Write-Output \"{WIN_EXIT_MARKER}$(if ($__fh_ok) {{0}} elseif ($LASTEXITCODE) {{$LASTEXITCODE}} else {{1}})\"\n",
        cwd = ps_quote(cwd),
    );
    let mut args: Vec<String> = extra.iter().map(|a| ps_quote(a)).collect();
    args.extend(
        [
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
        ]
        .iter()
        .map(|a| ps_quote(a)),
    );
    args.push(ps_quote(&encode_utf16_b64(&inner)));
    format!(
        "$ProgressPreference='SilentlyContinue'; $ErrorActionPreference='Continue'\n\
         $p=Start-Process -FilePath {exe} -ArgumentList {args} -NoNewWindow -PassThru\n\
         $null=$p.Handle\n\
         if (-not $p.WaitForExit({ms})) {{ taskkill /T /F /PID $p.Id 2>&1 | Out-Null; \
         $p.WaitForExit(); exit {TIMEOUT_EXIT} }}\n\
         exit $p.ExitCode\n",
        exe = ps_quote(exe),
        args = args.join(","),
        ms = timeout.saturating_mul(1000),
    )
}

/// Find the exit marker the wrapper printed, remove it (and anything
/// after it) from `stdout`, and return the status it encodes.
fn take_windows_exit_marker(stdout: &mut Vec<u8>) -> Option<i32> {
    let text = String::from_utf8_lossy(stdout);
    let pos = text.rfind(WIN_EXIT_MARKER)?;
    let rest = &text[pos + WIN_EXIT_MARKER.len()..];
    let line = rest.lines().next().unwrap_or("").trim();
    let code: i32 = line.parse().ok()?;
    // Keep only what came before the marker, minus trailing blank lines.
    let keep = text[..pos].trim_end_matches(['\r', '\n']).to_string();
    stdout.clear();
    stdout.extend_from_slice(keep.as_bytes());
    if !stdout.is_empty() {
        stdout.push(b'\n');
    }
    Some(code)
}

fn crlf_to_lf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\r' && data.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// PowerShell serialises its error stream as CLIXML when stderr is not a
/// console. Turn `<S S="Error">…</S>` records back into text, drop
/// progress records, and leave anything that is not CLIXML alone.
fn decode_clixml(data: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(data);
    if !text.contains("#< CLIXML") {
        return data.to_vec();
    }
    let mut out = String::new();
    let mut rest: &str = &text;
    loop {
        match rest.find("#< CLIXML") {
            None => {
                out.push_str(rest);
                break;
            }
            Some(h) => {
                out.push_str(&rest[..h]);
                let after = &rest[h + "#< CLIXML".len()..];
                let after = after
                    .strip_prefix("\r\n")
                    .or_else(|| after.strip_prefix('\n'))
                    .unwrap_or(after);
                match after.find("</Objs>") {
                    Some(end) => {
                        out.push_str(&clixml_strings(&after[..end]));
                        rest = &after[end + "</Objs>".len()..];
                    }
                    None => {
                        out.push_str(&clixml_strings(after));
                        break;
                    }
                }
            }
        }
    }
    out.into_bytes()
}

fn clixml_strings(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<S S=\"") {
        let after = &rest[start + 6..];
        let Some(q) = after.find('"') else { break };
        let kind = &after[..q];
        let after = &after[q + 1..];
        let Some(gt) = after.find('>') else { break };
        let body_start = &after[gt + 1..];
        let Some(end) = body_start.find("</S>") else {
            break;
        };
        if kind == "Error" || kind == "Warning" {
            out.push_str(&unescape_clixml(&body_start[..end]));
        }
        rest = &body_start[end + 4..];
    }
    out
}

fn unescape_clixml(s: &str) -> String {
    let s = s
        .replace("_x000D__x000A_", "\n")
        .replace("_x000A_", "\n")
        .replace("_x000D_", "")
        .replace("_x0009_", "\t");
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ---- paths -----------------------------------------------------------------

fn separator(p: Platform) -> char {
    match p {
        Platform::Posix => '/',
        Platform::Windows => '\\',
    }
}

/// `/C:/Users/me` — how Windows OpenSSH's sftp-server spells paths.
fn is_sftp_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':'
}

fn is_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn is_absolute_for(p: Platform, raw: &str) -> bool {
    match p {
        Platform::Posix => raw.starts_with('/'),
        Platform::Windows => {
            is_drive_path(raw) || is_sftp_drive_path(raw) || raw.starts_with("\\\\")
        }
    }
}

/// Native path for the SFTP layer: identity on POSIX, `C:\a\b` →
/// `/C:/a/b` on Windows.
fn to_sftp_path(p: Platform, native: &str) -> String {
    match p {
        Platform::Posix => native.to_string(),
        Platform::Windows => format!("/{}", native.replace('\\', "/")),
    }
}

/// `/C:/a/b` → `C:\a\b`.
fn from_sftp_path(sftp: &str) -> String {
    let s = sftp.strip_prefix('/').unwrap_or(sftp);
    let s = s.replace('/', "\\");
    if s.len() == 2 && is_drive_path(&s) {
        format!("{s}\\")
    } else {
        s
    }
}

fn parent_of(p: Platform, path: &str) -> Option<String> {
    match p {
        Platform::Posix => {
            if path == "/" {
                return None;
            }
            match path.rfind('/') {
                Some(0) => Some("/".to_string()),
                Some(i) => Some(path[..i].to_string()),
                None => None,
            }
        }
        Platform::Windows => {
            // `C:\` has no parent; `C:\a` → `C:\`.
            if path.len() <= 3 {
                return None;
            }
            let i = path.rfind('\\')?;
            if i <= 2 {
                Some(path[..3].to_string())
            } else {
                Some(path[..i].to_string())
            }
        }
    }
}

fn normalize_for(p: Platform, raw: &str) -> String {
    match p {
        Platform::Posix => normalize(raw),
        Platform::Windows => normalize_windows(raw),
    }
}

/// `~` and `~/x` become `home` and `home/x`; anything else is unchanged.
fn expand_tilde(p: &str, home: &str) -> String {
    if p == "~" {
        home.to_string()
    } else if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
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

/// Windows form with backslashes and a drive: accepts `C:/a/../b`,
/// `/C:/a/b` (SFTP form) and `C:\a\b`; UNC paths keep their `\\` prefix.
pub fn normalize_windows(p: &str) -> String {
    let unc = p.starts_with("\\\\") || p.starts_with("//");
    let s = p.replace('/', "\\");
    let s = s
        .strip_prefix('\\')
        .filter(|r| is_drive_path(r))
        .unwrap_or(&s);
    let mut parts: Vec<&str> = Vec::new();
    let mut drive = String::new();
    for (i, seg) in s.split('\\').enumerate() {
        if i == 0 && is_drive_path(seg) && seg.len() == 2 {
            drive = seg.to_uppercase();
            continue;
        }
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            x => parts.push(x),
        }
    }
    if unc {
        return format!("\\\\{}", parts.join("\\"));
    }
    if drive.is_empty() {
        // Drive-less absolute path: leave the caller a clear failure
        // rather than guessing a drive.
        return format!("\\{}", parts.join("\\"));
    }
    if parts.is_empty() {
        format!("{drive}\\")
    } else {
        format!("{drive}\\{}", parts.join("\\"))
    }
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
        assert_eq!(
            normalize_windows(&expand_tilde("~\\x", "C:\\Users\\me")),
            "C:\\Users\\me\\x"
        );
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(normalize("/../x"), "/x");
        assert_eq!(normalize("/"), "/");
    }

    #[test]
    fn windows_paths() {
        assert_eq!(normalize_windows("C:/Users/me/../x/./y"), "C:\\Users\\x\\y");
        assert_eq!(normalize_windows("/C:/Users/me"), "C:\\Users\\me");
        assert_eq!(normalize_windows("c:\\"), "C:\\");
        assert_eq!(
            normalize_windows("C:\\work/proj\\src"),
            "C:\\work\\proj\\src"
        );
        assert_eq!(
            normalize_windows("\\\\server\\share\\x"),
            "\\\\server\\share\\x"
        );
        assert_eq!(to_sftp_path(Platform::Windows, "C:\\a\\b"), "/C:/a/b");
        assert_eq!(from_sftp_path("/C:/Users/test"), "C:\\Users\\test");
        assert_eq!(from_sftp_path("/C:"), "C:\\");
        assert!(is_absolute_for(Platform::Windows, "D:\\x"));
        assert!(is_absolute_for(Platform::Windows, "/C:/x"));
        assert!(!is_absolute_for(Platform::Windows, "src\\main.rs"));
        assert!(!is_absolute_for(Platform::Windows, "/tmp"));
        assert_eq!(
            parent_of(Platform::Windows, "C:\\a\\b").as_deref(),
            Some("C:\\a")
        );
        assert_eq!(
            parent_of(Platform::Windows, "C:\\a").as_deref(),
            Some("C:\\")
        );
        assert_eq!(parent_of(Platform::Windows, "C:\\"), None);
        assert_eq!(parent_of(Platform::Posix, "/a/b").as_deref(), Some("/a"));
        assert_eq!(parent_of(Platform::Posix, "/a").as_deref(), Some("/"));
        assert_eq!(parent_of(Platform::Posix, "/"), None);
    }

    #[test]
    fn exit_marker_and_clixml() {
        let mut out = b"table\r\n__FARHAND_EXIT__=3\r\n\r\n".to_vec();
        assert_eq!(take_windows_exit_marker(&mut out), Some(3));
        assert_eq!(out, b"table\n");
        let mut out = b"x\n__FARHAND_EXIT__=1\n".to_vec();
        assert_eq!(take_windows_exit_marker(&mut out), Some(1));
        let mut out = b"__FARHAND_EXIT__=0\n".to_vec();
        assert_eq!(take_windows_exit_marker(&mut out), Some(0));
        assert!(out.is_empty());
        let mut out = b"no marker".to_vec();
        assert_eq!(take_windows_exit_marker(&mut out), None);

        let err = b"raw first\r\n#< CLIXML\r\n<Objs Version=\"1.1.0.1\"><Obj S=\"progress\"><TN></TN></Obj>\
                    <S S=\"Error\">Get-Item : Cannot find path &apos;C:\\nope&apos;_x000D__x000A_</S>\
                    <S S=\"Error\">At line:6_x000D__x000A_</S></Objs>";
        let text = String::from_utf8(decode_clixml(err)).unwrap();
        assert_eq!(
            text,
            "raw first\r\nGet-Item : Cannot find path 'C:\\nope'\nAt line:6\n"
        );
        assert_eq!(crlf_to_lf(b"a\r\nb\r\n"), b"a\nb\n");
        assert_eq!(ps_quote("it's"), "'it''s'");
    }

    #[test]
    fn wrapper_is_plain_ascii_on_the_command_line() {
        let w = windows_wrapper("powershell.exe", &[], "C:\\Users\\me", "Get-ChildItem", 30);
        assert!(w.contains("WaitForExit(30000)"));
        assert!(w.contains("-EncodedCommand"));
        assert!(encode_utf16_b64("x")
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)));
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
