//! `farhand` — the binary. Speaks MCP over stdio; logs go to stderr so the
//! protocol stream stays clean.
//!
//! ```text
//! farhand serve [--config PATH] [--cwd DIR]   run the MCP server (what agents launch)
//! farhand check [--config PATH] [--cwd DIR] [-v]   connect once and show the setup; -v adds the model's instructions
//! farhand validate [--config PATH] [--cwd DIR] [--json]   parse the config only; no network
//! farhand init                                print a configuration template
//! farhand install <agent> [--scope user|project] [--config PATH] [--yes|--dry-run]
//! farhand uninstall <agent> [--scope user|project] [--yes|--dry-run]
//! farhand status [--scope user|project]
//! farhand hook claude-code [--config PATH]    Claude Code PreToolUse hook (stdin JSON)
//! farhand hook codex [--config PATH]          Codex PreToolUse / SessionStart hook (stdin JSON)
//! farhand --version                           print the version
//! farhand -h | help [command]                 the commands, or one command's options
//! ```
//!
//! Agents: opencode, claude-code, codex.

mod hook;
mod install;
mod server;

use std::path::PathBuf;

use farhand_core::Config;
use rmcp::ServiceExt;

use crate::server::FarHand;

const DOCS: &str = "https://github.com/CogFlux/farhand/blob/main/docs/USAGE.md";

/// `farhand -h`: what each command is for, how a config is found, and
/// where the full documentation lives.
fn overview() -> String {
    format!(
        "farhand {version} — run a coding agent's tools on a remote host over SSH

usage: farhand <command> [options]       farhand <command> -h for its options

commands:
  serve       run the MCP server (what agents launch)
  check       connect once and show the setup, as the model will see it
  validate    parse the config only, no network; --json for agent entries
  init        print a configuration template
  install     wire FarHand into opencode, claude-code or codex
  uninstall   undo install
  status      show what is installed where
  hook        the hook command agent entries run (not for direct use)

config, first hit wins:
  --config PATH, $FARHAND_CONFIG, .farhand.toml in this folder or the
  nearest one above it, ~/.config/farhand/config.toml
  A file named .farhand.toml is a project file however it is found: it can
  only tighten the global config. Any other file passed with --config or
  $FARHAND_CONFIG is trusted like the global config.

The remote can be any SSH host, including a Docker sandbox (host
\"<name>.sbx\" after `sbx setup ssh`).

docs: {DOCS}
home: {home}",
        version = env!("CARGO_PKG_VERSION"),
        home = env!("CARGO_PKG_HOMEPAGE"),
    )
}

const CONFIG_OPT: &str =
    "  -c, --config PATH   use this config instead of resolving one per directory.
                      Named .farhand.toml it is a project file (may only
                      tighten the global config); any other name is trusted
                      like the global config";

/// `farhand <command> -h`. `None` for an unknown command.
fn command_help(command: &str) -> Option<String> {
    let text = match command {
        "serve" => format!(
            "usage: farhand serve [--config PATH] [--cwd DIR]

Run the MCP server on stdio. Agents launch this; you normally do not. In a
directory where FarHand is not active it offers no tools and says why.

{CONFIG_OPT}
  --cwd DIR           resolve the config for DIR instead of the working directory"
        ),
        "check" => format!(
            "usage: farhand check [--config PATH] [--cwd DIR] [-v]

Load the config, connect once, and show the host, workdir, platform,
approval and local folders. Warns when a Docker sandbox (`*.sbx`) has a
folder of this machine mounted read-write.

{CONFIG_OPT}
  --cwd DIR           check the config that applies in DIR
  -v, --verbose       also print the instructions the model receives"
        ),
        "validate" => format!(
            "usage: farhand validate [--config PATH] [--cwd DIR] [--json]

Parse the config without touching the network, and show which file applies,
whether FarHand is active, and the effective approval per tool. Settings a
project file was not allowed to make are listed as notes.

{CONFIG_OPT}
  --cwd DIR           resolve the config for DIR
  --json              one JSON object: ok, path, source (explicit, env,
                      project or global), trust (project: may only tighten;
                      global: trusted), active, host, workdir,
                      max_command_timeout_secs, notices, approval; on error
                      ok=false, code and error"
        ),
        "init" => "usage: farhand init

Print a commented configuration template, e.g.
  farhand init > .farhand.toml"
            .to_string(),
        "install" | "uninstall" => format!(
            "usage: farhand {command} <opencode|claude-code|codex> [--scope user|project]
                [--config PATH] [--yes|--dry-run]

Show the changes to the agent's own config files, ask, then make them.

  --scope user        every project of this user (default)
  --scope project     this directory only (.mcp.json, .codex/, .opencode/)
{CONFIG_OPT}
                      (install only: the path is baked into the agent entry,
                      and FarHand is active wherever that agent runs)
  -y, --yes           make the changes without asking
  --dry-run           show the changes, make none"
        ),
        "status" => "usage: farhand status [--scope user|project]

Show which agents have FarHand installed, at user scope (default) or in
this directory."
            .to_string(),
        "hook" => format!(
            "usage: farhand hook <claude-code|codex> [--config PATH]

The hook command `farhand install` writes into Claude Code and Codex. It
reads the agent's hook JSON on stdin, refuses local tools where FarHand is
active, and answers approval for FarHand's own tools.

{CONFIG_OPT}"
        ),
        _ => return None,
    };
    Some(format!("{text}\n\ndocs: {DOCS}"))
}

/// A malformed command line: the command's usage on stderr, exit 2.
fn usage(command: Option<&str>) -> ! {
    match command.and_then(command_help) {
        Some(h) => eprintln!("{h}"),
        None => eprintln!("{}", overview()),
    }
    std::process::exit(2);
}

fn help(command: Option<&str>) -> ! {
    match command {
        None => println!("{}", overview()),
        Some(c) => match command_help(c) {
            Some(h) => println!("{h}"),
            None => {
                eprintln!("farhand: unknown command `{c}`\n\n{}", overview());
                std::process::exit(2);
            }
        },
    }
    std::process::exit(0);
}

struct Args {
    command: String,
    target: Option<String>,
    config: Option<PathBuf>,
    cwd: Option<PathBuf>,
    json: bool,
    verbose: bool,
    scope: install::Scope,
    confirm: install::Confirm,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let command = it.next().unwrap_or_else(|| usage(None));
    match command.as_str() {
        "--version" | "-V" | "version" => {
            println!("farhand {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        "-h" | "--help" => help(None),
        "help" => help(it.next().as_deref()),
        c if command_help(c).is_none() => {
            eprintln!("farhand: unknown command `{c}`\n\n{}", overview());
            std::process::exit(2);
        }
        _ => {}
    }
    let bad = || -> ! { usage(Some(&command)) };
    let mut target = None;
    let mut config = None;
    let mut cwd = None;
    let mut json = false;
    let mut verbose = false;
    let mut scope = install::Scope::User;
    let mut confirm = install::Confirm::Ask;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => config = Some(PathBuf::from(it.next().unwrap_or_else(|| bad()))),
            "--cwd" => cwd = Some(PathBuf::from(it.next().unwrap_or_else(|| bad()))),
            "--json" => json = true,
            "-v" | "--verbose" => verbose = true,
            "-y" | "--yes" => confirm = install::Confirm::Yes,
            "--dry-run" => confirm = install::Confirm::DryRun,
            "--scope" => {
                scope = match it.next().as_deref() {
                    Some("user") => install::Scope::User,
                    Some("project") => install::Scope::Project,
                    _ => bad(),
                }
            }
            "-h" | "--help" => help(Some(&command)),
            s if !s.starts_with('-') && target.is_none() => target = Some(s.to_string()),
            _ => bad(),
        }
    }
    Args {
        command,
        target,
        config,
        cwd,
        json,
        verbose,
        scope,
        confirm,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FARHAND_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .init();

    let install_opts = || -> anyhow::Result<install::Options> {
        Ok(install::Options {
            scope: args.scope,
            confirm: args.confirm,
            config: match &args.config {
                Some(c) => Some(std::fs::canonicalize(c)?),
                None => None,
            },
            project: std::env::current_dir()?,
        })
    };

    match args.command.as_str() {
        "install" => {
            let agent = install::Agent::parse(
                args.target
                    .as_deref()
                    .unwrap_or_else(|| usage(Some(&args.command))),
            )?;
            install::install(agent, &install_opts()?)
        }
        "uninstall" => {
            let agent = install::Agent::parse(
                args.target
                    .as_deref()
                    .unwrap_or_else(|| usage(Some(&args.command))),
            )?;
            install::uninstall(agent, &install_opts()?)
        }
        "status" => install::status(&install_opts()?),
        "hook" => match args.target.as_deref() {
            Some("claude-code") | Some("claude") => hook::run_claude(args.config.as_deref()),
            Some("codex") => hook::run_codex(args.config.as_deref()),
            _ => usage(Some("hook")),
        },
        "init" => {
            print!("{}", Config::example());
            Ok(())
        }
        "validate" => {
            // Agent entries call this at startup; `--json` gives them the
            // facts they translate into their own permission systems.
            let loaded = Config::load_in(args.cwd.as_deref(), args.config.as_deref());
            if args.json {
                let out = match &loaded {
                    Ok(l) => serde_json::json!({
                        "ok": true,
                        "path": l.path,
                        "source": l.source.as_str(),
                        "trust": l.source.trust(),
                        "active": l.active(),
                        "host": l.config.remote.as_ref().map(|r| &r.host),
                        "workdir": l.config.remote.as_ref().map(|r| &r.workdir),
                        "max_command_timeout_secs": l.config.limits.max_command_timeout_secs,
                        "notices": l.notices,
                        "approval": l.config
                            .approval
                            .effective()
                            .iter()
                            .map(|(k, v)| (k.to_string(), serde_json::Value::from(v.as_str())))
                            .collect::<serde_json::Map<_, _>>(),
                    }),
                    Err(e) => serde_json::json!({
                        "ok": false,
                        "code": e.kind(),
                        "active": false,
                        "error": e.to_string(),
                    }),
                };
                println!("{out}");
                if loaded.is_err() {
                    std::process::exit(1);
                }
                return Ok(());
            }
            let l = loaded?;
            let remote = match &l.config.remote {
                Some(r) => format!("host `{}`, workdir `{}`", r.host, r.workdir),
                None => "no [remote]".to_string(),
            };
            println!(
                "ok: {} ({} file, {}, active here: {}, approval {})",
                l.path.display(),
                l.source.as_str(),
                remote,
                l.active(),
                l.config
                    .approval
                    .effective()
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            if l.source.trust() == "project" {
                println!(
                    "note: a .farhand.toml is a project file: it can only tighten the global config"
                );
            }
            for n in &l.notices {
                println!("note: {n}");
            }
            Ok(())
        }
        // A diagnostic for the user: does the config load, does the host
        // answer, what did it turn out to be, and what exactly will the
        // model be told. That last part is the MCP `instructions` text
        // verbatim; when the model misreads the setup, this is where to
        // look.
        "check" => {
            let l = Config::load_in(args.cwd.as_deref(), args.config.as_deref())?;
            let cfg = l.config.clone();
            println!(
                "config:     {} ({} file, active here: {})",
                l.path.display(),
                l.source.as_str(),
                l.active()
            );
            let Some(remote) = cfg.remote.clone() else {
                println!(
                    "remote:     none: this global config has no [remote], so FarHand is \
                     active only in folders with their own .farhand.toml"
                );
                return Ok(());
            };
            for n in &l.notices {
                println!("note:       {n}");
            }
            let server = FarHand::new(l.config, true)?;
            server.preflight().await?;
            let (platform, configured) = server.detected_platform().await?;
            println!(
                "remote:     {} — connected, {} platform",
                remote.host,
                platform.name()
            );
            let (workdir, created) = server.workdir_state().await?;
            println!(
                "workdir:    {}{}{}",
                remote.workdir,
                if workdir != remote.workdir {
                    format!(" (= {workdir})")
                } else {
                    String::new()
                },
                if created {
                    " — did not exist, created now"
                } else {
                    ""
                }
            );
            println!(
                "approval:   {}",
                cfg.approval
                    .effective()
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            println!(
                "local dirs: {}",
                if cfg.local.allowed_dirs.is_empty() {
                    "(none)".to_string()
                } else {
                    cfg.local
                        .allowed_dirs
                        .iter()
                        .map(|d| d.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
            if configured.is_none() {
                println!(
                    "hint:       add `os = \"{}\"` under [remote] so the model is told which \
                     shell it is writing for before the first command",
                    platform.name()
                );
            }
            // A Docker sandbox created with a workspace path shares that
            // folder with this machine read-write: the remote would be
            // writing straight onto the local disk.
            if is_sbx_host(&remote.host) {
                for share in server.writable_host_shares().await? {
                    println!(
                        "warning:    `{share}` is a folder of this machine mounted read-write \
                         into the sandbox: whatever the model writes there lands on the local \
                         disk at once, past download's checks. Recreate the sandbox without a \
                         workspace (`sbx create shell`) or with `--clone`"
                    );
                }
            }
            if args.verbose {
                println!(
                    "\n── instructions the model receives (verbatim, via MCP initialize) ──\n\n{}",
                    server.instructions()
                );
            } else {
                println!("\n(-v shows the instructions the model receives)");
            }
            Ok(())
        }
        "serve" => {
            // No config anywhere is not an error for a server an agent
            // launches in every directory: it is an inactive session.
            let server = match Config::load_in(args.cwd.as_deref(), args.config.as_deref()) {
                Ok(l) => {
                    let active = l.active();
                    let host = l.config.remote.as_ref().map(|r| r.host.as_str());
                    tracing::info!(config = %l.path.display(), source = l.source.as_str(), host, active, "farhand starting");
                    for n in &l.notices {
                        tracing::warn!("{n}");
                    }
                    if active {
                        FarHand::new(l.config, active)?
                    } else {
                        FarHand::inactive()
                    }
                }
                Err(farhand_core::Error::NoConfig) => {
                    tracing::info!("no config found; serving inactive");
                    FarHand::inactive()
                }
                Err(e) => return Err(e.into()),
            };
            let active = server.is_active();
            // Connect early so the first call does not wait for ssh and a
            // bad host shows in the log at startup; a failure here is
            // logged and retried lazily. It runs beside the MCP handshake,
            // not before it: agents list tools once, early (Codex at turn
            // start), and a waking sandbox can take seconds to answer ssh.
            // A call that arrives meanwhile waits for the same connection.
            // An inactive server offers no tools and does not touch the
            // network.
            if active {
                let early = server.clone();
                tokio::spawn(async move {
                    if let Err(e) = early.preflight().await {
                        tracing::warn!("preflight connection failed: {e}");
                    }
                });
            }
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
            Ok(())
        }
        _ => usage(None),
    }
}

/// Whether `host` names a Docker sandbox through the ssh config that
/// `sbx setup ssh` writes: `<name>.sbx`, or `<name>.sbx-<app>` for a named
/// app instance.
fn is_sbx_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    let name = h.rsplit_once('@').map_or(h.as_str(), |(_, n)| n);
    name.ends_with(".sbx") || name.contains(".sbx-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbx_hosts_are_recognised_by_name() {
        for h in ["demo.sbx", "Demo.SBX", "agent@demo.sbx", "demo.sbx-codex"] {
            assert!(is_sbx_host(h), "{h}");
        }
        for h in ["devbox", "sbx", "demo.sbxfoo", "user@host.example"] {
            assert!(!is_sbx_host(h), "{h}");
        }
    }
}
