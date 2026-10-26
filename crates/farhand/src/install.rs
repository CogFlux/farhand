//! `farhand install|uninstall|status <agent>` — one command per agent that
//! writes whatever that agent needs to run through FarHand. The agents'
//! extension mechanisms differ; this module hides the difference.
//!
//! | agent         | registers the MCP server        | closes local tools                          | second fence / approval                 |
//! |---------------|---------------------------------|---------------------------------------------|-----------------------------------------|
//! | `opencode`    | plugin file (embedded here)     | plugin: V1 `config` hook, V2 `tool.transform` | plugin `tool.execute.before` (V1 and V2) |
//! | `claude-code` | `claude mcp add` / `.mcp.json`  | `permissions.deny` (project scope only)     | `PreToolUse` hook (`farhand hook`)      |
//! | `codex`       | `[mcp_servers.farhand]` in TOML | `features.shell_tool = false` + read-only sandbox (project scope only) | `PreToolUse` hook (`farhand hook codex`); `tools.<t>.approval_mode` from `[approval]` |
//!
//! User scope never writes a static "local tools off" rule, because that
//! would turn every session on the machine remote; there the dynamic
//! pieces (the OpenCode plugin, the Claude Code and Codex hooks) decide per
//! directory from the config's `activation`. Project scope is already confined, so
//! it writes the static rules too.
//!
//! Every write is idempotent and reversible: install twice is a no-op,
//! uninstall removes exactly what install added and nothing else.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::hook::{CLAUDE_LOCAL_TOOLS, CODEX_LOCAL_TOOLS};

const OPENCODE_PLUGIN: &str = include_str!("../plugins/opencode/farhand.ts");
const PLUGIN_BIN_PLACEHOLDER: &str = "const BUILT_IN_BIN = \"farhand\"";
const PLUGIN_CONFIG_PLACEHOLDER: &str = "const BUILT_IN_CONFIG: string | undefined = undefined";
const HOOK_MARKER: &str = "hook claude-code";
const CODEX_HOOK_MARKER: &str = "hook codex";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    OpenCode,
    ClaudeCode,
    Codex,
}

impl Agent {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "opencode" => Ok(Agent::OpenCode),
            "claude-code" | "claude" => Ok(Agent::ClaudeCode),
            "codex" => Ok(Agent::Codex),
            _ => bail!("unknown agent `{s}`; expected opencode, claude-code or codex"),
        }
    }

    pub const ALL: &'static [Agent] = &[Agent::OpenCode, Agent::ClaudeCode, Agent::Codex];
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Agent::OpenCode => "opencode",
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

pub struct Options {
    pub scope: Scope,
    pub confirm: Confirm,
    /// Explicit config path baked into the agent's launch command.
    pub config: Option<PathBuf>,
    /// The project directory for `Scope::Project`.
    pub project: PathBuf,
}

/// Set by the npm wrapper to its own real path: the entry an agent should
/// launch. The binary it runs lives in a versioned folder that an upgrade
/// deletes, so that is not a path to write into an agent's config.
const LAUNCHER_ENV: &str = "FARHAND_LAUNCHER";

/// What an agent's config should launch: the npm wrapper when FarHand runs
/// through it, else this binary.
fn exe() -> Result<PathBuf> {
    launcher_from(
        std::env::var_os(LAUNCHER_ENV).filter(|v| !v.is_empty()),
        std::env::current_exe().context("cannot determine the farhand binary path")?,
    )
}

fn launcher_from(launcher: Option<std::ffi::OsString>, current: PathBuf) -> Result<PathBuf> {
    let Some(launcher) = launcher.map(PathBuf::from) else {
        return Ok(current);
    };
    // npx, `pnpm dlx` and bunx run packages from a cache that is cleared
    // behind the agent's back.
    let temporary = launcher.components().any(|c| {
        let c = c.as_os_str().to_string_lossy();
        c == "_npx" || c == "dlx" || c.starts_with("bunx-")
    });
    if temporary {
        bail!(
            "farhand is running from a temporary package cache ({}), and an agent set up from \
             it would lose FarHand when that cache is cleared. Install it for good first \
             (`npm i -g farhand`, or the install script in the README), then run \
             `farhand install` again.",
            launcher.display()
        );
    }
    Ok(launcher)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(fallback),
    }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

// ---- public entry points ----------------------------------------------------

pub fn install(agent: Agent, opts: &Options) -> Result<()> {
    let plan = match agent {
        Agent::OpenCode => plan_install_opencode(opts)?,
        Agent::ClaudeCode => plan_install_claude(opts)?,
        Agent::Codex => plan_install_codex(opts)?,
    };
    carry_out(&plan, opts)
}

pub fn uninstall(agent: Agent, opts: &Options) -> Result<()> {
    let plan = match agent {
        Agent::OpenCode => plan_uninstall_opencode()?,
        Agent::ClaudeCode => plan_uninstall_claude(opts)?,
        Agent::Codex => plan_uninstall_codex(opts)?,
    };
    carry_out(&plan, opts)
}

// ---- plans ------------------------------------------------------------------
//
// `install` and `uninstall` work out every change first, show it — a diff
// for each file, the command line for each agent CLI call — and make it
// only once the user agrees, or passed `--yes`. `--dry-run` only shows.

/// How `install` and `uninstall` go from a plan to changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// Show the plan and ask; refuse without a terminal to ask on.
    Ask,
    /// Show the plan and apply it.
    Yes,
    /// Show the plan only.
    DryRun,
}

enum Change {
    /// Write `after` to `path`; `before` is what is there now. With a
    /// `summary`, that is shown instead of the diff.
    Write {
        path: PathBuf,
        before: Option<String>,
        after: String,
        summary: Option<&'static str>,
    },
    Remove {
        path: PathBuf,
    },
    /// Run the agent's own CLI, for files that it owns.
    RunClaude {
        args: Vec<String>,
    },
}

struct Plan {
    title: String,
    changes: Vec<Change>,
    /// Shown with the plan: what is already in place or left alone.
    notes: Vec<String>,
    /// Shown once the changes are made: what the user still has to do.
    next: Vec<String>,
}

impl Plan {
    fn new(verb: &str, agent: Agent, opts: &Options) -> Self {
        let scope = match opts.scope {
            Scope::User => "user scope",
            Scope::Project => "project scope",
        };
        Plan {
            title: format!("farhand {verb} {agent} ({scope})"),
            changes: Vec::new(),
            notes: Vec::new(),
            next: Vec::new(),
        }
    }

    /// Add a write of `after` to `path`, unless the file already holds it.
    fn write(
        &mut self,
        path: PathBuf,
        after: String,
        summary: Option<&'static str>,
    ) -> Result<bool> {
        let before = if path.is_file() {
            Some(std::fs::read_to_string(&path)?)
        } else {
            None
        };
        if before.as_deref() == Some(after.as_str()) {
            self.notes
                .push(format!("{} is already up to date", path.display()));
            return Ok(false);
        }
        self.changes.push(Change::Write {
            path,
            before,
            after,
            summary,
        });
        Ok(true)
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if self.changes.is_empty() {
            out.push_str(&format!("{}: nothing to change\n", self.title));
        } else {
            out.push_str(&format!("{} will make these changes:\n", self.title));
        }
        for change in &self.changes {
            match change {
                Change::Write {
                    path,
                    before,
                    after,
                    summary,
                } => {
                    let verb = if before.is_some() { "modify" } else { "create" };
                    out.push_str(&format!("\n  {verb} {}\n", path.display()));
                    match summary {
                        Some(s) => out.push_str(&format!("    {s}\n")),
                        None => {
                            let old = before.as_deref().unwrap_or("");
                            let diff = similar::TextDiff::from_lines(old, after.as_str());
                            let unified = diff.unified_diff().context_radius(2).to_string();
                            for line in unified.lines() {
                                out.push_str(&format!("    {line}\n"));
                            }
                        }
                    }
                }
                Change::Remove { path } => {
                    out.push_str(&format!("\n  delete {}\n", path.display()));
                }
                Change::RunClaude { args } => {
                    let line: Vec<String> = args.iter().map(|a| shell_word(a)).collect();
                    out.push_str(&format!("\n  run: claude {}\n", line.join(" ")));
                }
            }
        }
        for note in &self.notes {
            out.push_str(&format!("  ({note})\n"));
        }
        out
    }

    fn apply(&self) -> Result<()> {
        for change in &self.changes {
            match change {
                Change::Write { path, after, .. } => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(path, after)
                        .with_context(|| format!("cannot write {}", path.display()))?;
                }
                Change::Remove { path } => std::fs::remove_file(path)
                    .with_context(|| format!("cannot delete {}", path.display()))?,
                Change::RunClaude { args } => {
                    let out = claude_cli()?.args(args).output()?;
                    if !out.status.success() {
                        bail!(
                            "`claude {}` failed: {}",
                            args.join(" "),
                            String::from_utf8_lossy(&out.stderr).trim()
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// Show the plan, then apply it if `opts.confirm` and the user allow.
fn carry_out(plan: &Plan, opts: &Options) -> Result<()> {
    print!("{}", plan.render());
    if plan.changes.is_empty() {
        return Ok(());
    }
    let go = match opts.confirm {
        Confirm::Yes => true,
        Confirm::DryRun => {
            println!("\ndry run: nothing was changed");
            false
        }
        Confirm::Ask => ask_to_apply()?,
    };
    if !go {
        return Ok(());
    }
    plan.apply()?;
    println!("\ndone.");
    for line in &plan.next {
        println!("- {line}");
    }
    Ok(())
}

fn ask_to_apply() -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        bail!(
            "nothing was changed: there is no terminal to confirm on. Rerun with --yes to make \
             the changes above, or --dry-run to only show them."
        );
    }
    print!("\nApply these changes? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    let yes = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !yes {
        println!("nothing was changed");
    }
    Ok(yes)
}

pub fn status(opts: &Options) -> Result<()> {
    println!(
        "farhand {} ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe()?.display()
    );
    for agent in Agent::ALL {
        let lines = match agent {
            Agent::OpenCode => status_opencode(opts)?,
            Agent::ClaudeCode => status_claude(opts)?,
            Agent::Codex => status_codex(opts)?,
        };
        println!("{agent}");
        for l in lines {
            println!("  {l}");
        }
    }
    Ok(())
}

// ---- OpenCode ---------------------------------------------------------------

fn opencode_plugin_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
        .join("opencode")
        .join("plugins")
        .join("farhand.ts")
}

fn rendered_plugin(opts: &Options) -> Result<String> {
    let bin = exe()?.display().to_string();
    if !OPENCODE_PLUGIN.contains(PLUGIN_BIN_PLACEHOLDER)
        || !OPENCODE_PLUGIN.contains(PLUGIN_CONFIG_PLACEHOLDER)
    {
        bail!("embedded plugin lacks the BUILT_IN_* placeholders");
    }
    let config = match &opts.config {
        Some(c) => format!(
            "const BUILT_IN_CONFIG: string | undefined = {}",
            json!(c.display().to_string())
        ),
        None => PLUGIN_CONFIG_PLACEHOLDER.to_string(),
    };
    Ok(OPENCODE_PLUGIN
        .replace(
            PLUGIN_BIN_PLACEHOLDER,
            &format!("const BUILT_IN_BIN = {}", json!(bin)),
        )
        .replace(PLUGIN_CONFIG_PLACEHOLDER, &config))
}

fn plan_install_opencode(opts: &Options) -> Result<Plan> {
    let mut plan = Plan::new("install", Agent::OpenCode, opts);
    let path = opencode_plugin_path();
    if path.is_symlink() {
        plan.notes.push(format!(
            "{} is a symlink (development install); left alone",
            path.display()
        ));
        return Ok(plan);
    }
    let summary = "FarHand's OpenCode plugin: registers the farhand MCP server, turns \
                   OpenCode's local tools off wherever FarHand is active, and tells the model \
                   it works remotely";
    if plan.write(path, rendered_plugin(opts)?, Some(summary))? {
        plan.next
            .push("restart OpenCode; tools appear as farhand_remote_shell etc.".into());
    }
    Ok(plan)
}

fn plan_uninstall_opencode() -> Result<Plan> {
    let opts = Options {
        scope: Scope::User,
        confirm: Confirm::Ask,
        config: None,
        project: PathBuf::new(),
    };
    let mut plan = Plan::new("uninstall", Agent::OpenCode, &opts);
    let path = opencode_plugin_path();
    if path.is_symlink() || path.exists() {
        plan.changes.push(Change::Remove { path });
    } else {
        plan.notes.push("the plugin is not installed".into());
    }
    Ok(plan)
}

fn status_opencode(opts: &Options) -> Result<Vec<String>> {
    let path = opencode_plugin_path();
    Ok(vec![if path.is_symlink() {
        format!(
            "plugin: symlink at {} (development install)",
            path.display()
        )
    } else if path.is_file() {
        let current = std::fs::read_to_string(&path)? == rendered_plugin(opts)?;
        format!(
            "plugin: installed at {}{}",
            path.display(),
            if current {
                ""
            } else {
                " (outdated; run `farhand install opencode`)"
            }
        )
    } else {
        "plugin: not installed".to_string()
    }])
}

// ---- Claude Code --------------------------------------------------------------

fn claude_settings_path(opts: &Options) -> PathBuf {
    match opts.scope {
        Scope::User => home().join(".claude").join("settings.json"),
        Scope::Project => opts.project.join(".claude").join("settings.json"),
    }
}

fn read_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))
}

/// The hook must resolve the same config as the server, so an explicit
/// `--config` is baked into both.
fn hook_command(opts: &Options) -> Result<String> {
    agent_hook_command(opts, HOOK_MARKER)
}

fn agent_hook_command(opts: &Options, marker: &str) -> Result<String> {
    let mut cmd = format!("{} {marker}", shell_word(&exe()?.display().to_string()));
    if let Some(c) = &opts.config {
        cmd.push_str(&format!(
            " --config {}",
            shell_word(&c.display().to_string())
        ));
    }
    Ok(cmd)
}

/// Single-quote a word for the shell Claude Code runs hook commands in.
fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn hook_matcher() -> String {
    format!("{}|mcp__farhand__.*", CLAUDE_LOCAL_TOOLS.join("|"))
}

fn is_farhand_hook_group(group: &Value) -> bool {
    group["hooks"]
        .as_array()
        .map(|hs| {
            hs.iter().any(|h| {
                h["command"]
                    .as_str()
                    .is_some_and(|c| c.contains(HOOK_MARKER))
            })
        })
        .unwrap_or(false)
}

/// Add FarHand's hook — and, with `static_deny`, its deny list — to a
/// settings document. Returns whether anything changed.
fn apply_claude_settings(settings: &mut Value, static_deny: bool, opts: &Options) -> Result<bool> {
    let mut changed = false;
    if !settings.is_object() {
        *settings = json!({});
    }

    if static_deny {
        // permissions.deny ∪ local tools
        let perms = settings
            .as_object_mut()
            .unwrap()
            .entry("permissions")
            .or_insert_with(|| json!({}));
        if !perms.is_object() {
            *perms = json!({});
        }
        let deny = perms
            .as_object_mut()
            .unwrap()
            .entry("deny")
            .or_insert_with(|| json!([]));
        if !deny.is_array() {
            *deny = json!([]);
        }
        let deny_arr = deny.as_array_mut().unwrap();
        for t in CLAUDE_LOCAL_TOOLS {
            if !deny_arr.iter().any(|v| v.as_str() == Some(t)) {
                deny_arr.push(json!(t));
                changed = true;
            }
        }
    }

    // hooks.PreToolUse: one FarHand group. Claude Code lets a call through
    // when its hook errors or times out; `onFailure: block` refuses it
    // instead, so a missing binary or a slow config cannot open the fence.
    let wanted = json!({
        "matcher": hook_matcher(),
        "hooks": [{
            "type": "command",
            "command": hook_command(opts)?,
            "timeout": 10,
            "onFailure": "block",
        }],
    });
    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let pre = hooks
        .as_object_mut()
        .unwrap()
        .entry("PreToolUse")
        .or_insert_with(|| json!([]));
    if !pre.is_array() {
        *pre = json!([]);
    }
    let groups = pre.as_array_mut().unwrap();
    match groups.iter().position(is_farhand_hook_group) {
        Some(i) if groups[i] == wanted => {}
        Some(i) => {
            groups[i] = wanted;
            changed = true;
        }
        None => {
            groups.push(wanted);
            changed = true;
        }
    }
    Ok(changed)
}

/// Remove what `apply_claude_settings` added. Returns whether anything changed.
/// A local-tool deny entry the user had written themselves before installing
/// is indistinguishable from ours and goes too; the uninstall output says so.
fn strip_claude_settings(settings: &mut Value) -> bool {
    let mut changed = false;
    if let Some(deny) = settings
        .pointer_mut("/permissions/deny")
        .and_then(Value::as_array_mut)
    {
        let before = deny.len();
        deny.retain(|v| !v.as_str().is_some_and(|s| CLAUDE_LOCAL_TOOLS.contains(&s)));
        changed |= deny.len() != before;
    }
    if let Some(pre) = settings
        .pointer_mut("/hooks/PreToolUse")
        .and_then(Value::as_array_mut)
    {
        let before = pre.len();
        pre.retain(|g| !is_farhand_hook_group(g));
        changed |= pre.len() != before;
    }
    changed
}

fn serve_command(opts: &Options) -> Result<Vec<String>> {
    let mut cmd = vec![exe()?.display().to_string(), "serve".to_string()];
    if let Some(c) = &opts.config {
        cmd.push("--config".into());
        cmd.push(c.display().to_string());
    }
    Ok(cmd)
}

fn claude_cli() -> Result<Command> {
    let mut c = Command::new("claude");
    c.stdin(std::process::Stdio::null());
    Ok(c)
}

fn claude_mcp_registered() -> Result<bool> {
    let out = claude_cli()?
        .args(["mcp", "get", "farhand"])
        .output()
        .context("cannot run `claude`; is Claude Code installed?")?;
    Ok(out.status.success())
}

fn json_text(value: &Value) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(text)
}

/// The `farhand` entry `claude mcp add --scope user` wrote, read without
/// touching the file: `claude mcp` owns it.
fn claude_user_server() -> Option<Value> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(home);
    let doc = read_json(&dir.join(".claude.json")).ok()?;
    doc.pointer("/mcpServers/farhand").cloned()
}

fn plan_install_claude(opts: &Options) -> Result<Plan> {
    let mut plan = Plan::new("install", Agent::ClaudeCode, opts);
    let serve = serve_command(opts)?;
    match opts.scope {
        Scope::User => {
            // `claude mcp add` owns ~/.claude.json; never edit that file by hand.
            let current = claude_user_server();
            let same = current
                .as_ref()
                .is_some_and(|c| c["command"] == json!(serve[0]) && c["args"] == json!(serve[1..]));
            if same {
                plan.notes
                    .push("the MCP server `farhand` is already registered".into());
            } else {
                if current.is_some() {
                    plan.changes.push(Change::RunClaude {
                        args: ["mcp", "remove", "--scope", "user", "farhand"]
                            .map(String::from)
                            .to_vec(),
                    });
                }
                let mut args: Vec<String> = ["mcp", "add", "--scope", "user", "farhand", "--"]
                    .map(String::from)
                    .to_vec();
                args.extend(serve.iter().cloned());
                plan.changes.push(Change::RunClaude { args });
            }
        }
        Scope::Project => {
            let path = opts.project.join(".mcp.json");
            let mut doc = read_json(&path)?;
            if !doc.is_object() {
                doc = json!({});
            }
            let servers = doc
                .as_object_mut()
                .unwrap()
                .entry("mcpServers")
                .or_insert_with(|| json!({}));
            if !servers.is_object() {
                *servers = json!({});
            }
            servers["farhand"] = json!({ "command": serve[0], "args": serve[1..] });
            plan.write(path, json_text(&doc)?, None)?;
            plan.next
                .push("Claude Code asks you to approve the project's .mcp.json once".into());
        }
    }

    let path = claude_settings_path(opts);
    let static_deny = opts.scope == Scope::Project;
    let mut settings = read_json(&path)?;
    if apply_claude_settings(&mut settings, static_deny, opts)? {
        plan.write(path, json_text(&settings)?, None)?;
    } else {
        plan.notes
            .push(format!("{} is already up to date", path.display()));
    }
    if !plan.changes.is_empty() {
        plan.next.push(match opts.scope {
            Scope::User => "the hook refuses Claude Code's local tools wherever FarHand is \
                            active; restart Claude Code"
                .into(),
            Scope::Project => format!(
                "{} are denied in this project; restart Claude Code",
                CLAUDE_LOCAL_TOOLS.join(", ")
            ),
        });
        plan.next
            .push("approval follows [approval] in the FarHand config at call time".into());
    }
    Ok(plan)
}

fn plan_uninstall_claude(opts: &Options) -> Result<Plan> {
    let mut plan = Plan::new("uninstall", Agent::ClaudeCode, opts);
    match opts.scope {
        Scope::User => {
            if claude_user_server().is_some() {
                plan.changes.push(Change::RunClaude {
                    args: ["mcp", "remove", "--scope", "user", "farhand"]
                        .map(String::from)
                        .to_vec(),
                });
            } else {
                plan.notes
                    .push("the MCP server `farhand` is not registered at user scope".into());
            }
        }
        Scope::Project => {
            let path = opts.project.join(".mcp.json");
            let mut doc = read_json(&path)?;
            if let Some(servers) = doc.get_mut("mcpServers").and_then(Value::as_object_mut) {
                if servers.remove("farhand").is_some() {
                    plan.write(path, json_text(&doc)?, None)?;
                }
            }
        }
    }
    let path = claude_settings_path(opts);
    if path.exists() {
        let mut settings = read_json(&path)?;
        if strip_claude_settings(&mut settings) {
            plan.write(path, json_text(&settings)?, None)?;
            plan.notes.push(format!(
                "a {} deny entry you wrote yourself is removed too",
                CLAUDE_LOCAL_TOOLS.join(" / ")
            ));
        }
    }
    Ok(plan)
}

fn status_claude(opts: &Options) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let registered = match opts.scope {
        Scope::User => claude_mcp_registered().unwrap_or(false),
        Scope::Project => read_json(&opts.project.join(".mcp.json"))
            .map(|d| d["mcpServers"]["farhand"].is_object())
            .unwrap_or(false),
    };
    lines.push(format!(
        "mcp server: {}",
        if registered {
            "registered"
        } else {
            "not registered"
        }
    ));
    let path = claude_settings_path(opts);
    let settings = read_json(&path).unwrap_or(json!({}));
    let denied = settings["permissions"]["deny"]
        .as_array()
        .map(|d| {
            CLAUDE_LOCAL_TOOLS
                .iter()
                .all(|t| d.iter().any(|v| v.as_str() == Some(t)))
        })
        .unwrap_or(false);
    let hooked = settings["hooks"]["PreToolUse"]
        .as_array()
        .map(|g| g.iter().any(is_farhand_hook_group))
        .unwrap_or(false);
    lines.push(format!(
        "PreToolUse hook: {hooked}, static deny list: {denied}{} ({})",
        if opts.scope == Scope::User {
            " (user scope relies on the hook)"
        } else {
            ""
        },
        path.display()
    ));
    Ok(lines)
}

// ---- Codex ------------------------------------------------------------------
//
// Codex reads `~/.codex/config.toml` and, for trusted projects,
// `<project>/.codex/config.toml`. It can switch its shell tool off
// (`features.shell_tool`), sandbox the rest read-only, and approve MCP tools
// per tool — enough for a real FarHand session at project scope. Its hooks
// can deny a call but not ask, so `[approval]` is translated when you run
// `install` (run it again after changing it), and the hook only refuses
// local tools and, at session start, tells the model it works remotely.
// That makes user scope a FarHand session too, once the user has trusted
// the hooks in Codex (`/hooks`): Codex runs no hook it has not been shown.

fn codex_config_path(opts: &Options) -> PathBuf {
    match opts.scope {
        Scope::User => home().join(".codex").join("config.toml"),
        Scope::Project => opts.project.join(".codex").join("config.toml"),
    }
}

fn read_toml(path: &Path) -> Result<toml_edit::DocumentMut> {
    let text = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    text.parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))
}

fn implicit_table() -> toml_edit::Item {
    let mut t = toml_edit::Table::new();
    t.set_implicit(true); // no bare header, only the subtables
    toml_edit::Item::Table(t)
}

fn plan_install_codex(opts: &Options) -> Result<Plan> {
    let mut plan = Plan::new("install", Agent::Codex, opts);
    let path = codex_config_path(opts);
    let mut doc = read_toml(&path)?;
    let before = doc.to_string();

    let loaded = farhand_core::Config::load_in(Some(&opts.project), opts.config.as_deref())
        .context("codex needs the FarHand config to translate [approval]")?;
    let serve = serve_command(opts)?;

    let mut server = toml_edit::Table::new();
    server["command"] = toml_edit::value(serve[0].clone());
    let mut args = toml_edit::Array::new();
    for a in &serve[1..] {
        args.push(a.clone());
    }
    server["args"] = toml_edit::value(args);
    server["startup_timeout_sec"] = toml_edit::value(60);
    server["default_tools_approval_mode"] = toml_edit::value("prompt");
    let mut tools = toml_edit::Table::new();
    tools.set_implicit(true);
    for (tool, mode) in loaded.config.approval.effective() {
        let mut t = toml_edit::Table::new();
        t["approval_mode"] = toml_edit::value(match mode {
            farhand_core::config::ApprovalMode::Auto => "auto",
            _ => "prompt",
        });
        tools[tool] = toml_edit::Item::Table(t);
    }
    server["tools"] = toml_edit::Item::Table(tools);

    if !doc.contains_key("mcp_servers") {
        doc["mcp_servers"] = implicit_table();
    }
    doc["mcp_servers"]["farhand"] = toml_edit::Item::Table(server);

    apply_codex_hooks(&mut doc, &agent_hook_command(opts, CODEX_HOOK_MARKER)?)?;

    if opts.scope == Scope::Project {
        if !doc.contains_key("features") {
            doc["features"] = implicit_table();
        }
        doc["features"]["shell_tool"] = toml_edit::value(false);
        doc["features"]["unified_exec"] = toml_edit::value(false);
        doc["sandbox_mode"] = toml_edit::value("read-only");
    }

    let after = doc.to_string();
    if after == before {
        plan.notes
            .push(format!("{} is already up to date", path.display()));
        return Ok(plan);
    }
    plan.write(path, after, None)?;
    plan.next.push(
        "Codex runs a new or changed hook only after you trust it: start Codex, open /hooks \
         and trust FarHand's PreToolUse and SessionStart hooks"
            .into(),
    );
    plan.next.push(match opts.scope {
        Scope::Project => "local shell off and sandbox read-only for this project; Codex loads \
                           .codex/config.toml only after you trust the project"
            .into(),
        Scope::User => format!(
            "once trusted, the hook refuses {} wherever FarHand is active (it needs Codex \
             0.164 or later; on older versions use --scope project)",
            CODEX_LOCAL_TOOLS.join(", ")
        ),
    });
    plan.next
        .push("[approval] was translated now; rerun install after changing it".into());
    Ok(plan)
}

fn plan_uninstall_codex(opts: &Options) -> Result<Plan> {
    let mut plan = Plan::new("uninstall", Agent::Codex, opts);
    let path = codex_config_path(opts);
    if !path.exists() {
        plan.notes
            .push(format!("{} does not exist", path.display()));
        return Ok(plan);
    }
    let mut doc = read_toml(&path)?;
    let before = doc.to_string();
    if let Some(servers) = doc.get_mut("mcp_servers").and_then(|s| s.as_table_mut()) {
        servers.remove("farhand");
    }
    if doc
        .get("mcp_servers")
        .and_then(|s| s.as_table())
        .is_some_and(|t| t.is_empty())
    {
        doc.remove("mcp_servers");
    }
    strip_codex_hooks(&mut doc);
    if opts.scope == Scope::Project {
        if let Some(f) = doc.get_mut("features").and_then(|f| f.as_table_mut()) {
            f.remove("shell_tool");
            f.remove("unified_exec");
        }
        if doc
            .get("features")
            .and_then(|f| f.as_table())
            .is_some_and(|t| t.is_empty())
        {
            doc.remove("features");
        }
        if doc.get("sandbox_mode").and_then(|v| v.as_str()) == Some("read-only") {
            doc.remove("sandbox_mode");
        }
    }
    let after = doc.to_string();
    if after == before {
        plan.notes
            .push(format!("nothing of FarHand's is in {}", path.display()));
    } else {
        plan.write(path, after, None)?;
    }
    Ok(plan)
}

fn status_codex(opts: &Options) -> Result<Vec<String>> {
    let path = codex_config_path(opts);
    let doc = if path.exists() {
        read_toml(&path).ok()
    } else {
        None
    };
    let present = doc
        .as_ref()
        .and_then(|d| d.get("mcp_servers").and_then(|s| s.get("farhand")))
        .is_some();
    let shell_off = doc
        .as_ref()
        .and_then(|d| d.get("features").and_then(|f| f.get("shell_tool")))
        .and_then(|v| v.as_bool())
        == Some(false);
    let hooked = doc.as_ref().is_some_and(|d| {
        CODEX_HOOK_EVENTS.iter().all(|event| {
            d.get("hooks")
                .and_then(|h| h.get(event))
                .and_then(|g| g.as_array_of_tables())
                .is_some_and(|groups| groups.iter().any(is_codex_hook_group))
        })
    });
    Ok(vec![format!(
        "mcp server: {}, local shell off: {shell_off}, hooks: {} ({})",
        if present {
            "registered"
        } else {
            "not registered"
        },
        if hooked {
            "installed (trust them in Codex's /hooks)"
        } else {
            "not installed"
        },
        path.display()
    )])
}

/// The hook events FarHand registers with Codex.
const CODEX_HOOK_EVENTS: &[&str] = &["PreToolUse", "SessionStart"];

fn is_codex_hook_group(group: &toml_edit::Table) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array_of_tables())
        .is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(CODEX_HOOK_MARKER))
            })
        })
}

/// The `[[hooks.<event>]]` array, converting an inline `<event> = [...]`
/// the user may have written into the same array-of-tables form.
fn codex_hook_groups<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    event: &str,
) -> Result<&'a mut toml_edit::ArrayOfTables> {
    if !doc.contains_key("hooks") {
        doc["hooks"] = implicit_table();
    }
    let hooks = doc["hooks"]
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("`hooks` in the Codex config is not a table"))?;
    let item = hooks
        .entry(event)
        .or_insert(toml_edit::Item::ArrayOfTables(Default::default()));
    if !item.is_array_of_tables() {
        let not_tables = || anyhow!("`hooks.{event}` in the Codex config is not a list of tables");
        let list = item.as_array().ok_or_else(not_tables)?;
        let mut groups = toml_edit::ArrayOfTables::new();
        for v in list.iter() {
            groups.push(
                v.as_inline_table()
                    .ok_or_else(not_tables)?
                    .clone()
                    .into_table(),
            );
        }
        *item = toml_edit::Item::ArrayOfTables(groups);
    }
    Ok(item.as_array_of_tables_mut().unwrap())
}

/// Add FarHand's `PreToolUse` and `SessionStart` groups, or bring them up to
/// date in place: Codex keys a hook's trust by its position.
fn apply_codex_hooks(doc: &mut toml_edit::DocumentMut, command: &str) -> Result<()> {
    for event in CODEX_HOOK_EVENTS {
        let mut handler = toml_edit::Table::new();
        handler["type"] = toml_edit::value("command");
        handler["command"] = toml_edit::value(command);
        handler["timeout"] = toml_edit::value(10);
        // Codex shows this as the hook's name in /hooks and while it runs.
        handler["statusMessage"] = toml_edit::value(match *event {
            "PreToolUse" => "FarHand: refuse local tools",
            _ => "FarHand: tell the model it works remotely",
        });
        let mut handlers = toml_edit::ArrayOfTables::new();
        handlers.push(handler);
        let mut group = toml_edit::Table::new();
        if *event == "PreToolUse" {
            group["matcher"] = toml_edit::value(CODEX_LOCAL_TOOLS.join("|"));
        }
        group["hooks"] = toml_edit::Item::ArrayOfTables(handlers);

        let groups = codex_hook_groups(doc, event)?;
        let ours = groups.iter().position(is_codex_hook_group);
        match ours {
            Some(i) => {
                let existing = groups.get_mut(i).unwrap();
                if codex_group_fields(existing) != codex_group_fields(&group) {
                    *existing = group;
                }
            }
            None => groups.push(group),
        }
    }
    Ok(())
}

/// A hook group's settings, nested handlers included, without formatting.
/// (`Table`'s `Display` leaves out nested arrays of tables.)
fn codex_group_fields(group: &toml_edit::Table) -> Vec<(String, String)> {
    let value = |v: &toml_edit::Item| v.as_value().map(|v| v.to_string().trim().to_string());
    let mut fields = Vec::new();
    for (key, item) in group.iter() {
        match item.as_array_of_tables() {
            Some(handlers) => {
                for (i, h) in handlers.iter().enumerate() {
                    for (k, v) in h.iter() {
                        fields.push((format!("{key}.{i}.{k}"), value(v).unwrap_or_default()));
                    }
                }
            }
            None => fields.push((key.to_string(), value(item).unwrap_or_default())),
        }
    }
    fields
}

/// Remove what `apply_codex_hooks` added, and the tables it leaves empty.
fn strip_codex_hooks(doc: &mut toml_edit::DocumentMut) {
    let Some(hooks) = doc.get_mut("hooks").and_then(|h| h.as_table_like_mut()) else {
        return;
    };
    for event in CODEX_HOOK_EVENTS {
        if let Some(groups) = hooks
            .get_mut(event)
            .and_then(|g| g.as_array_of_tables_mut())
        {
            groups.retain(|g| !is_codex_hook_group(g));
            if groups.is_empty() {
                hooks.remove(event);
            }
        }
    }
    if hooks.is_empty() {
        doc.remove("hooks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_launch_the_npm_entry_not_its_versioned_binary() {
        let bin = PathBuf::from("/x/node_modules/farhand/vendor/farhand-0.2.10-t/farhand");
        assert_eq!(launcher_from(None, bin.clone()).unwrap(), bin);
        let entry = "/opt/homebrew/lib/node_modules/farhand/bin/farhand.js";
        assert_eq!(
            launcher_from(Some(entry.into()), bin.clone()).unwrap(),
            PathBuf::from(entry)
        );
        for cached in [
            "/Users/u/.npm/_npx/0123abcd/node_modules/farhand/bin/farhand.js",
            "/Users/u/Library/Caches/pnpm/dlx/abc/node_modules/farhand/bin/farhand.js",
            "/tmp/bunx-501-farhand@latest/node_modules/farhand/bin/farhand.js",
        ] {
            let err = launcher_from(Some(cached.into()), bin.clone()).unwrap_err();
            assert!(err.to_string().contains("npm i -g farhand"), "{err}");
        }
    }

    fn opts() -> Options {
        Options {
            scope: Scope::Project,
            confirm: Confirm::Ask,
            config: None,
            project: PathBuf::from("/tmp/p"),
        }
    }

    #[test]
    fn a_plan_shows_each_change_before_making_it() {
        let d = tempfile::tempdir().unwrap();
        let existing = d.path().join("settings.json");
        std::fs::write(&existing, "{\n  \"model\": \"opus\"\n}\n").unwrap();
        let same = d.path().join("same.json");
        std::fs::write(&same, "{}\n").unwrap();

        let mut plan = Plan::new("install", Agent::ClaudeCode, &opts());
        let after = "{\n  \"model\": \"opus\",\n  \"hooks\": {}\n}\n".to_string();
        assert!(plan.write(existing.clone(), after.clone(), None).unwrap());
        assert!(!plan.write(same.clone(), "{}\n".into(), None).unwrap());
        plan.write(d.path().join("new.ts"), "x\n".into(), Some("a plugin"))
            .unwrap();
        plan.changes.push(Change::RunClaude {
            args: vec!["mcp".into(), "add".into(), "a b".into()],
        });
        let shown = plan.render();
        assert!(shown.starts_with("farhand install claude-code (project scope) will make"));
        assert!(shown.contains(&format!("modify {}", existing.display())));
        assert!(shown.contains("+  \"hooks\": {}"), "{shown}");
        assert!(shown.contains("-  \"model\": \"opus\"\n"), "{shown}");
        assert!(shown.contains("create ") && shown.contains("    a plugin"));
        assert!(shown.contains("run: claude mcp add 'a b'"));
        assert!(shown.contains(&format!("({} is already up to date)", same.display())));

        // A dry run shows the plan and leaves every file as it was.
        let dry = Options {
            confirm: Confirm::DryRun,
            ..opts()
        };
        let files_only = Plan {
            changes: plan
                .changes
                .into_iter()
                .filter(|c| !matches!(c, Change::RunClaude { .. }))
                .collect(),
            ..Plan::new("install", Agent::ClaudeCode, &dry)
        };
        carry_out(&files_only, &dry).unwrap();
        assert!(!std::fs::read_to_string(&existing)
            .unwrap()
            .contains("hooks"));
        assert!(!d.path().join("new.ts").exists());

        let yes = Options {
            confirm: Confirm::Yes,
            ..opts()
        };
        carry_out(&files_only, &yes).unwrap();
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), after);
        assert!(d.path().join("new.ts").exists());
    }

    #[test]
    fn an_empty_plan_needs_no_confirmation() {
        let plan = Plan::new("uninstall", Agent::Codex, &opts());
        assert!(plan.render().contains("nothing to change"));
        // Ask without a terminal would refuse; with nothing to do it never asks.
        carry_out(&plan, &opts()).unwrap();
    }

    #[test]
    fn claude_settings_round_trip() {
        let mut s = json!({
            "model": "opus",
            "permissions": { "allow": ["WebFetch"], "deny": ["WebSearch"] },
            "hooks": { "PreToolUse": [ { "matcher": "Write", "hooks": [ { "type": "command", "command": "lint" } ] } ] }
        });
        let original = s.clone();
        assert!(apply_claude_settings(&mut s, true, &opts()).unwrap());
        let deny = s["permissions"]["deny"].as_array().unwrap();
        assert_eq!(deny.len(), CLAUDE_LOCAL_TOOLS.len() + 1);
        assert_eq!(s["permissions"]["allow"], json!(["WebFetch"]));
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        // idempotent
        assert!(!apply_claude_settings(&mut s, true, &opts()).unwrap());
        // reversible, other content untouched
        assert!(strip_claude_settings(&mut s));
        assert_eq!(s, original);
    }

    #[test]
    fn claude_user_scope_writes_only_the_hook() {
        let mut s = json!({});
        assert!(apply_claude_settings(&mut s, false, &opts()).unwrap());
        assert!(s.get("permissions").is_none());
        assert!(s["hooks"]["PreToolUse"][0].is_object());
        // A hook that fails must refuse the call, not wave it through.
        assert_eq!(
            s["hooks"]["PreToolUse"][0]["hooks"][0]["onFailure"],
            "block"
        );
    }

    #[test]
    fn claude_settings_from_empty() {
        let mut s = json!({});
        assert!(apply_claude_settings(&mut s, true, &opts()).unwrap());
        assert!(s["hooks"]["PreToolUse"][0]["matcher"]
            .as_str()
            .unwrap()
            .contains("mcp__farhand__.*"));
        assert!(strip_claude_settings(&mut s));
        assert_eq!(s["permissions"]["deny"], json!([]));
    }

    #[test]
    fn codex_hooks_round_trip() {
        let original = "model = \"gpt\"\n\n[[hooks.PreToolUse]]\nmatcher = \"Bash\"\n\n\
                        [[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"lint\"\n";
        let mut doc: toml_edit::DocumentMut = original.parse().unwrap();
        apply_codex_hooks(&mut doc, "/bin/farhand hook codex").unwrap();
        let pre = doc["hooks"]["PreToolUse"].as_array_of_tables().unwrap();
        assert_eq!(pre.len(), 2);
        // The user's group keeps its place; Codex keys hook trust by position.
        assert_eq!(pre.get(0).unwrap()["matcher"].as_str(), Some("Bash"));
        let ours = pre.get(1).unwrap();
        assert_eq!(
            ours["matcher"].as_str(),
            Some(CODEX_LOCAL_TOOLS.join("|").as_str())
        );
        assert_eq!(ours["hooks"][0]["timeout"].as_integer(), Some(10));
        assert!(doc["hooks"]["SessionStart"][0].get("matcher").is_none());
        assert_eq!(
            ours["hooks"][0]["statusMessage"].as_str(),
            Some("FarHand: refuse local tools")
        );
        // idempotent
        let once = doc.to_string();
        apply_codex_hooks(&mut doc, "/bin/farhand hook codex").unwrap();
        assert_eq!(doc.to_string(), once);
        // a changed command is updated in place, not added again
        apply_codex_hooks(&mut doc, "/new/farhand hook codex").unwrap();
        let pre = doc["hooks"]["PreToolUse"].as_array_of_tables().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(
            pre.get(1).unwrap()["hooks"][0]["command"].as_str(),
            Some("/new/farhand hook codex")
        );
        // reversible
        strip_codex_hooks(&mut doc);
        assert_eq!(doc.to_string(), original);
    }

    #[test]
    fn codex_hooks_accept_an_inline_list_and_leave_nothing_behind() {
        let mut doc: toml_edit::DocumentMut = "[hooks]\nPreToolUse = []\n".parse().unwrap();
        apply_codex_hooks(&mut doc, "farhand hook codex").unwrap();
        assert_eq!(
            doc["hooks"]["PreToolUse"]
                .as_array_of_tables()
                .unwrap()
                .len(),
            1
        );
        let reparsed: toml_edit::DocumentMut = doc.to_string().parse().unwrap();
        assert_eq!(
            reparsed["hooks"]["PreToolUse"][0]["hooks"][0]["type"].as_str(),
            Some("command")
        );
        strip_codex_hooks(&mut doc);
        assert!(doc.get("hooks").is_none());
    }

    #[test]
    fn plugin_placeholders_are_rendered() {
        assert!(OPENCODE_PLUGIN.contains(PLUGIN_BIN_PLACEHOLDER));
        assert!(OPENCODE_PLUGIN.contains(PLUGIN_CONFIG_PLACEHOLDER));
        let plain = rendered_plugin(&opts()).unwrap();
        assert!(!plain.contains(PLUGIN_BIN_PLACEHOLDER));
        assert!(plain.contains(PLUGIN_CONFIG_PLACEHOLDER));
        let with = rendered_plugin(&Options {
            config: Some(PathBuf::from("/etc/fh.toml")),
            ..opts()
        })
        .unwrap();
        assert!(with.contains("const BUILT_IN_CONFIG: string | undefined = \"/etc/fh.toml\""));
    }

    #[test]
    fn hook_command_carries_config() {
        let cmd = hook_command(&Options {
            config: Some(PathBuf::from("/my dir/fh.toml")),
            ..opts()
        })
        .unwrap();
        assert!(cmd.ends_with("hook claude-code --config '/my dir/fh.toml'"));
        assert!(hook_command(&opts()).unwrap().ends_with("hook claude-code"));
    }
}
