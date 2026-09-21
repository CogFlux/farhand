//! `farhand` — the binary. Speaks MCP over stdio; logs go to stderr so the
//! protocol stream stays clean.
//!
//! ```text
//! farhand serve [--config PATH]               run the MCP server (what agents launch)
//! farhand check [--config PATH] [-v]          connect once and show the setup; -v adds the model's instructions
//! farhand validate [--config PATH] [--cwd DIR] [--json]   parse the config only; no network
//! farhand init                                print a configuration template
//! farhand install <agent> [--scope user|project] [--config PATH]
//! farhand uninstall <agent> [--scope user|project]
//! farhand status [--scope user|project]
//! farhand hook claude-code [--config PATH]    Claude Code PreToolUse hook (stdin JSON)
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

fn usage() -> ! {
    eprintln!(
        "usage:\n  farhand serve [--config PATH]\n  farhand check [--config PATH] [-v]\n  \
         farhand validate [--config PATH] [--cwd DIR] [--json]\n  farhand init\n  \
         farhand install <opencode|claude-code|codex> [--scope user|project] [--config PATH]\n  \
         farhand uninstall <opencode|claude-code|codex> [--scope user|project]\n  \
         farhand status [--scope user|project]\n  \
         farhand hook claude-code [--config PATH]"
    );
    std::process::exit(2);
}

struct Args {
    command: String,
    target: Option<String>,
    config: Option<PathBuf>,
    cwd: Option<PathBuf>,
    json: bool,
    verbose: bool,
    scope: install::Scope,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let command = it.next().unwrap_or_else(|| usage());
    let mut target = None;
    let mut config = None;
    let mut cwd = None;
    let mut json = false;
    let mut verbose = false;
    let mut scope = install::Scope::User;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => config = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--cwd" => cwd = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--json" => json = true,
            "-v" | "--verbose" => verbose = true,
            "--scope" => {
                scope = match it.next().as_deref() {
                    Some("user") => install::Scope::User,
                    Some("project") => install::Scope::Project,
                    _ => usage(),
                }
            }
            "-h" | "--help" => usage(),
            s if !s.starts_with('-') && target.is_none() => target = Some(s.to_string()),
            _ => usage(),
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
            config: match &args.config {
                Some(c) => Some(std::fs::canonicalize(c)?),
                None => None,
            },
            project: std::env::current_dir()?,
        })
    };

    match args.command.as_str() {
        "install" => {
            let agent = install::Agent::parse(args.target.as_deref().unwrap_or_else(|| usage()))?;
            install::install(agent, &install_opts()?)
        }
        "uninstall" => {
            let agent = install::Agent::parse(args.target.as_deref().unwrap_or_else(|| usage()))?;
            install::uninstall(agent, &install_opts()?)
        }
        "status" => install::status(&install_opts()?),
        "hook" => match args.target.as_deref() {
            Some("claude-code") | Some("claude") => hook::run(args.config.as_deref()),
            _ => usage(),
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
                        "active": l.active(),
                        "host": l.config.remote.host,
                        "workdir": l.config.remote.workdir,
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
            println!(
                "ok: {} (host `{}`, workdir `{}`, active here: {}, approval {})",
                l.path.display(),
                l.config.remote.host,
                l.config.remote.workdir,
                l.active(),
                l.config
                    .approval
                    .effective()
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
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
                "config:     {} (active here: {})",
                l.path.display(),
                l.active()
            );
            let server = FarHand::new(l.config, true)?;
            server.preflight().await?;
            let (platform, configured) = server.detected_platform().await?;
            println!(
                "remote:     {} — connected, {} platform",
                cfg.remote.host,
                platform.name()
            );
            println!("workdir:    {}", cfg.remote.workdir);
            println!(
                "approval:   {}",
                format!("{:?}", cfg.approval.mode).to_lowercase()
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
                    tracing::info!(config = %l.path.display(), host = %l.config.remote.host, active, "farhand starting");
                    FarHand::new(l.config, active)?
                }
                Err(farhand_core::Error::NoConfig) => {
                    tracing::info!("no config found; serving inactive");
                    FarHand::inactive()
                }
                Err(e) => return Err(e.into()),
            };
            let active = server.is_active();
            // Connect eagerly so a bad host fails loudly at startup instead
            // of on the model's first call; a failure here is logged and
            // retried lazily. An inactive server offers no tools and does
            // not touch the network.
            if active {
                if let Err(e) = server.preflight().await {
                    tracing::warn!("preflight connection failed: {e}");
                }
            }
            let service = server.serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
            Ok(())
        }
        _ => usage(),
    }
}
