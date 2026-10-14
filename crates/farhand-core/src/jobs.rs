//! Long-running processes on the remote: a dev server, a watcher, anything
//! that must outlive one `remote_shell` call.
//!
//! A command lives as long as its SSH channel. On Windows OpenSSH every
//! process the session started is killed when the channel closes, even one
//! detached with `Start-Process`; on POSIX a background child that keeps
//! the output open holds the command until its timeout, which then kills
//! the whole process group. So a job is started outside the session — in
//! its own session with `setsid` on POSIX, by WMI (`Win32_Process.Create`)
//! on Windows — with its output in a log file, and is recorded under
//! `~/.farhand/jobs/` on the remote so a later FarHand session can still
//! list and stop it.

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::remote::{encode_utf16_b64, ps_quote, separator, Platform, Remote};
use crate::text::shell_quote;

/// Seconds a launcher, status check or stop may take.
const CONTROL_TIMEOUT: u64 = 30;
/// Exit status a job reports when it cannot enter its directory.
const CD_FAILED_EXIT: i32 = 97;

/// What is recorded about a job on the remote, as `<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub started: String,
    /// Native path of the file the job's stdout and stderr go to.
    pub log: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    /// Ended by itself with this status.
    Exited(i32),
    /// Not running and left no status: stopped by `remote_kill`, killed
    /// otherwise, or the remote rebooted.
    Gone,
}

impl Remote {
    /// `~/.farhand/jobs` on the remote, native form.
    async fn jobs_dir(&self) -> Result<String> {
        let home = self.home().await?;
        let sep = separator(self.platform().await?);
        Ok(format!("{home}{sep}.farhand{sep}jobs"))
    }

    /// Start `command` in the background, in `cwd` (default: the workdir),
    /// detached from the SSH session. Returns the job as recorded.
    pub async fn spawn(&self, command: &str, cwd: Option<&str>) -> Result<Job> {
        let platform = self.platform().await?;
        let cwd = match cwd {
            Some(c) => self.resolve(c).await?,
            None => self.effective_workdir().await?,
        };
        let dir = self.jobs_dir().await?;
        self.ensure_dir(&dir).await?;
        let id = format!("{:08x}", crate::remote::random_u64() as u32);
        let sep = separator(platform);
        let log = format!("{dir}{sep}{id}.log");
        let status = format!("{dir}{sep}{id}.exit");
        let launcher = match platform {
            Platform::Posix => posix_launcher(self.shell(), command, &cwd, &log, &status),
            Platform::Windows => {
                let (exe, extra) = self.powershell();
                windows_launcher(exe, &extra, command, &cwd, &log, &status)
            }
        };
        let out = self.exec(&launcher, None, Some(CONTROL_TIMEOUT)).await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let pid = stdout
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pid="))
            .find_map(|p| p.trim().parse::<u32>().ok())
            .ok_or_else(|| {
                Error::Ssh(format!(
                    "the job did not start (exit code {}): {}{}",
                    out.exit_code,
                    stdout.trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            })?;
        let job = Job {
            id,
            pid,
            command: command.to_string(),
            cwd,
            started: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            log,
        };
        let record = serde_json::to_vec_pretty(&job).map_err(std::io::Error::other)?;
        self.write_file(&format!("{dir}{sep}{}.json", job.id), &record)
            .await?;
        Ok(job)
    }

    /// Every recorded job, newest first, with its state.
    pub async fn jobs(&self) -> Result<Vec<(Job, JobState)>> {
        let dir = self.jobs_dir().await?;
        let sep = separator(self.platform().await?);
        let mut jobs = Vec::new();
        for name in self.list_files(&dir).await? {
            if !name.ends_with(".json") {
                continue;
            }
            let Ok((bytes, _)) = self
                .read_file(&format!("{dir}{sep}{name}"), 64 * 1024)
                .await
            else {
                continue;
            };
            if let Ok(job) = serde_json::from_slice::<Job>(&bytes) {
                jobs.push(job);
            }
        }
        jobs.sort_by(|a, b| b.started.cmp(&a.started));
        let running = self.running(&jobs).await?;
        let mut out = Vec::with_capacity(jobs.len());
        for job in jobs {
            let state = if running.contains(&job.pid) {
                JobState::Running
            } else {
                match self
                    .read_file(&format!("{dir}{sep}{}.exit", job.id), 64)
                    .await
                {
                    Ok((b, _)) => String::from_utf8_lossy(&b)
                        .trim()
                        .parse()
                        .map(JobState::Exited)
                        .unwrap_or(JobState::Gone),
                    Err(_) => JobState::Gone,
                }
            };
            out.push((job, state));
        }
        Ok(out)
    }

    /// Stop the job `id` and the processes it started. Returns the job and
    /// whether it was running.
    pub async fn kill_job(&self, id: &str) -> Result<(Job, bool)> {
        if id.len() != 8 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid(format!(
                "`{id}` is not a job id (see remote_jobs)"
            )));
        }
        let (job, state) = self
            .jobs()
            .await?
            .into_iter()
            .find(|(j, _)| j.id == id)
            .ok_or_else(|| Error::Invalid(format!("no job `{id}` (see remote_jobs)")))?;
        if state != JobState::Running {
            return Ok((job, false));
        }
        let script = match self.platform().await? {
            // The job leads its own process group when `setsid` or `perl`
            // was there to start it; signal the group, then the process.
            Platform::Posix => format!(
                "kill -TERM -- -{p} 2>/dev/null || kill -TERM {p} 2>/dev/null\n\
                 for _ in 1 2 3 4 5; do kill -0 {p} 2>/dev/null || exit 0; sleep 1; done\n\
                 kill -KILL -- -{p} 2>/dev/null || kill -KILL {p} 2>/dev/null\nexit 0\n",
                p = job.pid
            ),
            Platform::Windows => format!("taskkill /T /F /PID {} 2>&1 | Out-Null", job.pid),
        };
        self.exec(&script, None, Some(CONTROL_TIMEOUT)).await?;
        Ok((job, true))
    }

    /// Which of the jobs' processes are alive, in one round trip.
    async fn running(&self, jobs: &[Job]) -> Result<Vec<u32>> {
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let pids = jobs.iter().map(|j| j.pid.to_string()).collect::<Vec<_>>();
        let script = match self.platform().await? {
            Platform::Posix => format!(
                "for p in {}; do kill -0 $p 2>/dev/null && echo $p; done; exit 0",
                pids.join(" ")
            ),
            Platform::Windows => format!(
                "foreach ($p in @({})) {{ if (Get-Process -Id $p -ErrorAction \
                 SilentlyContinue) {{ $p }} }}",
                pids.join(",")
            ),
        };
        let out = self.exec(&script, None, Some(CONTROL_TIMEOUT)).await?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect())
    }
}

/// The POSIX launcher: runs `command` with the configured shell in a new
/// session (`setsid`, or `perl`'s `setpgrp` where there is none, as on
/// macOS), output to `log`, status to `status`, and prints `pid=<pid>`.
fn posix_launcher(shell: &[String], command: &str, cwd: &str, log: &str, status: &str) -> String {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    let shell = shell
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let job = format!(
        "cd {cwd} || {{ echo {CD_FAILED_EXIT} > {status}; exit {CD_FAILED_EXIT}; }}\n\
         export NO_COLOR=1\n\
         {shell} \"$(printf %s {cmd} | base64 -d)\"\n\
         echo $? > {status}\n",
        cwd = shell_quote(cwd),
        status = shell_quote(status),
        cmd = b64(command),
    );
    format!(
        "detach() {{\n\
         \x20 if command -v setsid >/dev/null 2>&1; then exec setsid nohup \"$@\"\n\
         \x20 elif command -v perl >/dev/null 2>&1; then \
         exec perl -e 'setpgrp(0, 0); exec @ARGV or die \"exec: $!\"' nohup \"$@\"\n\
         \x20 else exec nohup \"$@\"; fi\n\
         }}\n\
         ( detach sh -c \"$(printf %s {job} | base64 -d)\" ) > {log} 2>&1 < /dev/null &\n\
         echo pid=$!\n",
        job = b64(&job),
        log = shell_quote(log),
    )
}

/// The Windows launcher: has WMI start `cmd.exe`, which runs the command
/// in PowerShell with its output in `log`; WMI's process is not in the
/// SSH session's job object, so it survives the session. Prints
/// `pid=<pid>` of that `cmd.exe`, the root of the job's process tree.
fn windows_launcher(
    exe: &str,
    extra: &[String],
    command: &str,
    cwd: &str,
    log: &str,
    status: &str,
) -> String {
    let job = format!(
        "$ProgressPreference='SilentlyContinue'; $ErrorActionPreference='Continue'\n\
         [Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8\n\
         $env:NO_COLOR='1'; $env:PYTHONUTF8='1'; $env:PYTHONIOENCODING='utf-8'\n\
         try {{ Set-Location -LiteralPath {cwd} -ErrorAction Stop }} \
         catch {{ Set-Content -LiteralPath {status} -Value {CD_FAILED_EXIT}; exit {CD_FAILED_EXIT} }}\n\
         $global:LASTEXITCODE=0\n\
         {command}\n\
         $__fh_ok=$?\n\
         $__fh_code=$(if ($__fh_ok) {{0}} elseif ($LASTEXITCODE) {{$LASTEXITCODE}} \
         elseif ($Error.Count -and $Error[0].FullyQualifiedErrorId -like 'NativeCommandError*') {{0}} \
         else {{1}})\n\
         Set-Content -LiteralPath {status} -Value $__fh_code\n\
         exit $__fh_code\n",
        cwd = ps_quote(cwd),
        status = ps_quote(status),
    );
    let extra = extra
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(" ");
    // `/s` makes cmd strip only the outermost quotes and keep the rest.
    let line = format!(
        "cmd.exe /d /s /c \"\"{exe}\" {extra} -NoProfile -NonInteractive -ExecutionPolicy Bypass \
         -EncodedCommand {enc} > \"{log}\" 2>&1\"",
        enc = encode_utf16_b64(&job),
    );
    format!(
        "$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments \
         @{{ CommandLine = {line}; CurrentDirectory = {cwd} }}\n\
         if ($r.ReturnValue -ne 0) {{ [Console]::Error.WriteLine(\"Win32_Process.Create \
         returned $($r.ReturnValue)\"); exit 1 }}\n\
         \"pid=$($r.ProcessId)\"\n",
        line = ps_quote(&line),
        cwd = ps_quote(cwd),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launcher run locally, with a real `sh`: the job outlives the
    /// launcher, writes its log and status, and leads its own group.
    #[cfg(unix)]
    #[test]
    fn a_posix_job_outlives_its_launcher() {
        let d = tempfile::tempdir().unwrap();
        let p = |n: &str| d.path().join(n).to_string_lossy().into_owned();
        let script = posix_launcher(
            &["sh".into(), "-c".into()],
            "sleep 1; echo \"out $NO_COLOR\"; exit 3",
            &d.path().to_string_lossy(),
            &p("job.log"),
            &p("job.exit"),
        );
        let started = std::time::Instant::now();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        // The launcher returns at once: the job does not hold its output.
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let text = String::from_utf8_lossy(&out.stdout);
        let pid: i32 = text.trim().strip_prefix("pid=").unwrap().parse().unwrap();
        // A process group of its own (once it has got that far), so
        // remote_kill can stop its children.
        let mut pgid = 0;
        for _ in 0..50 {
            pgid = unsafe { libc::getpgid(pid) };
            if pgid == pid {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(pgid, pid, "{text}");
        for _ in 0..50 {
            if std::path::Path::new(&p("job.exit")).exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(std::fs::read_to_string(p("job.exit")).unwrap().trim(), "3");
        assert_eq!(std::fs::read_to_string(p("job.log")).unwrap(), "out 1\n");
    }

    #[test]
    fn the_windows_launcher_goes_through_wmi() {
        let s = windows_launcher(
            "powershell.exe",
            &[],
            "npm run dev",
            "C:\\Users\\me\\proj",
            "C:\\Users\\me\\.farhand\\jobs\\0badc0de.log",
            "C:\\Users\\me\\.farhand\\jobs\\0badc0de.exit",
        );
        assert!(s.contains("Win32_Process -MethodName Create"), "{s}");
        assert!(
            s.contains("CurrentDirectory = 'C:\\Users\\me\\proj'"),
            "{s}"
        );
        assert!(
            s.contains("> \"C:\\Users\\me\\.farhand\\jobs\\0badc0de.log\" 2>&1"),
            "{s}"
        );
        // The command itself travels encoded, never on the command line.
        assert!(!s.contains("npm run dev"), "{s}");
    }
}
