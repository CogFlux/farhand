//! A one-shot password delivery from a user prompt to a remote process.

use std::io::{self, IsTerminal, Write};
use std::time::Instant;

use farhand_core::audit::{Audit, Record};
use farhand_core::config::Loaded;
use farhand_core::guard::Guard;
use farhand_core::remote::{ExecOutput, Remote};
use farhand_core::{Error, Result};
use zeroize::Zeroizing;

#[cfg(target_os = "macos")]
mod dialog;

#[cfg(target_os = "macos")]
pub async fn prompt_desktop(host: &str, workdir: &str, command: &str) -> Result<Zeroizing<String>> {
    use tokio::sync::Semaphore;

    static DIALOG: Semaphore = Semaphore::const_new(1);
    let permit = DIALOG
        .try_acquire()
        .map_err(|_| Error::Denied("another FarHand password dialog is already open".into()))?;
    let details = format!("Host {host:?}\nWorking directory {workdir:?}\nCommand {command:?}");
    if command.trim().is_empty() || details.chars().count() > 480 {
        return Err(Error::Invalid(
            "the remote command must be short enough to review in the password dialog".into(),
        ));
    }
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        dialog::request(&details)
    })
    .await
    .map_err(|_| Error::Denied("the password dialog could not finish".into()))?
}

#[cfg(not(target_os = "macos"))]
pub async fn prompt_desktop(_: &str, _: &str, _: &str) -> Result<Zeroizing<String>> {
    Err(Error::Denied(
        "the desktop password prompt requires macOS; use farhand secret from a local terminal"
            .into(),
    ))
}

pub async fn run(loaded: Loaded, command: &str) -> anyhow::Result<()> {
    anyhow::ensure!(loaded.active(), "FarHand is not active here");
    let audit_dir = loaded.config.audit_dir();
    let cfg = loaded
        .config
        .remote
        .ok_or_else(|| Error::Config("missing [remote]".into()))?;
    let guard = Guard::new(&loaded.config.guard)?;
    let audit = Audit::open(audit_dir, &cfg.host)?;
    let started = Instant::now();
    let result = async {
        guard.ensure_content("the command", command.as_bytes())?;
        guard.ensure_content("the remote host", cfg.host.as_bytes())?;
        guard.ensure_content("the remote workdir", cfg.workdir.as_bytes())?;
        let password = prompt(&cfg.host, &cfg.workdir, command)?;
        let remote = Remote::new(cfg, loaded.config.limits);
        remote
            .exec_with_password(&guard, command, None, None, password.as_bytes())
            .await
    }
    .await;

    let mut record = Record::new("secret");
    record.duration_ms = started.elapsed().as_millis();
    record.outcome = match &result {
        Ok(out) if out.timed_out => "timeout",
        Ok(out) if out.exit_code == 0 => "ok",
        Err(Error::Denied(_)) => "denied",
        Err(Error::Timeout { .. }) => "timeout",
        _ => "error",
    };
    record.exit_code = result.as_ref().ok().map(|out| out.exit_code);
    audit.record(record);
    if let Some(error) = audit.failure() {
        anyhow::bail!("{error}");
    }
    let ExecOutput {
        exit_code,
        timed_out,
        ..
    } = result?;
    anyhow::ensure!(!timed_out, "the remote command timed out");
    anyhow::ensure!(
        exit_code == 0,
        "the remote command exited with code {exit_code}"
    );
    println!("Remote command completed (exit code 0).");
    Ok(())
}

fn prompt(host: &str, workdir: &str, command: &str) -> Result<Zeroizing<String>> {
    if command.trim().is_empty() {
        return Err(Error::Invalid("the remote command is empty".into()));
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(Error::Denied(
            "run farhand secret in a local interactive terminal".into(),
        ));
    }
    eprint!(
        "Send a password to the standard input of this remote command?\n\
         Host {host:?}\nWorking directory {workdir:?}\nCommand {command:?}\nContinue? [y/N] "
    );
    io::stderr().flush()?;
    let mut answer = Zeroizing::new(String::new());
    io::stdin().read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") && !answer.trim().eq_ignore_ascii_case("yes") {
        return Err(Error::Denied("password delivery was not approved".into()));
    }
    let terminal = rpassword::ConfigBuilder::new()
        .output_writer(io::stderr())
        .build();
    Ok(Zeroizing::new(rpassword::prompt_password_with_config(
        "Password: ",
        terminal,
    )?))
}
