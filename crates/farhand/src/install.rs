//! `farhand install|uninstall|status <agent>` — one command per agent that
//! writes whatever that agent needs to run through FarHand. The agents'
//! extension mechanisms differ; this module hides the difference.
//!
//! | agent         | registers the MCP server        | closes local tools                          | second fence / approval                 |
//! |---------------|---------------------------------|---------------------------------------------|-----------------------------------------|
//! | `opencode`    | plugin file (embedded here)     | plugin: V1 `config` hook, V2 `tool.transform` | plugin `tool.execute.before` (V1 and V2) |
//! | `claude-code` | `claude mcp add` / `.mcp.json`  | `permissions.deny` (project scope only)     | `PreToolUse` hook (`farhand hook`)      |
//! | `codex`       | `[mcp_servers.farhand]` in TOML | `features.shell_tool = false` + read-only sandbox (project scope only) | `tools.<t>.approval_mode` from `[approval]` |
//!
//! User scope never writes a static "local tools off" rule, because that
//! would turn every session on the machine remote; there the dynamic
//! pieces (the OpenCode plugin, the Claude Code hook) decide per directory
//! from the config's `activation`. Project scope is already confined, so
//! it writes the static rules too.
//!
//! Every write is idempotent and reversible: install twice is a no-op,
//! uninstall removes exactly what install added and nothing else.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::hook::CLAUDE_LOCAL_TOOLS;

const OPENCODE_PLUGIN: &str = include_str!("../plugins/opencode/farhand.ts");
const PLUGIN_BIN_PLACEHOLDER: &str = "const BUILT_IN_BIN = \"farhand\"";
const PLUGIN_CONFIG_PLACEHOLDER: &str = "const BUILT_IN_CONFIG: string | undefined = undefined";
const HOOK_MARKER: &str = "hook claude-code";

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
    /// Explicit config path baked into the agent's launch command.
    pub config: Option<PathBuf>,
    /// The project directory for `Scope::Project`.
    pub project: PathBuf,
}

fn exe() -> Result<PathBuf> {
    std::env::current_exe().context("cannot determine the farhand binary path")
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
    match agent {
        Agent::OpenCode => install_opencode(opts),
        Agent::ClaudeCode => install_claude(opts),
        Agent::Codex => install_codex(opts),
    }
}

pub fn uninstall(agent: Agent, opts: &Options) -> Result<()> {
    match agent {
        Agent::OpenCode => uninstall_opencode(),
        Agent::ClaudeCode => uninstall_claude(opts),
        Agent::Codex => uninstall_codex(opts),
    }
}

pub fn status(opts: &Options) -> Result<()> {
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

fn install_opencode(opts: &Options) -> Result<()> {
    let path = opencode_plugin_path();
    if path.is_symlink() {
        println!(
            "opencode: {} is a symlink (development install); leaving it alone",
            path.display()
        );
        return Ok(());
    }
    let content = rendered_plugin(opts)?;
    if path.is_file() && std::fs::read_to_string(&path)? == content {
        println!("opencode: plugin already up to date at {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, content)?;
    println!("opencode: wrote plugin to {}", path.display());
    println!("opencode: restart OpenCode; tools appear as farhand_remote_shell etc.");
    Ok(())
}

fn uninstall_opencode() -> Result<()> {
    let path = opencode_plugin_path();
    if path.is_symlink() || path.exists() {
        std::fs::remove_file(&path)?;
        println!("opencode: removed {}", path.display());
    } else {
        println!("opencode: nothing installed");
    }
    Ok(())
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

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text)?;
    Ok(())
}

/// The hook must resolve the same config as the server, so an explicit
/// `--config` is baked into both.
fn hook_command(opts: &Options) -> Result<String> {
    let mut cmd = format!("{} {HOOK_MARKER}", exe()?.display());
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

    // hooks.PreToolUse: one FarHand group
    let wanted = json!({
        "matcher": hook_matcher(),
        "hooks": [{ "type": "command", "command": hook_command(opts)?, "timeout": 10 }],
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

fn install_claude(opts: &Options) -> Result<()> {
    let serve = serve_command(opts)?;
    match opts.scope {
        Scope::User => {
            // `claude mcp add` owns ~/.claude.json; never edit that file by hand.
            if claude_mcp_registered()? {
                let _ = claude_cli()?
                    .args(["mcp", "remove", "--scope", "user", "farhand"])
                    .output();
            }
            let mut args: Vec<String> = vec![
                "mcp".into(),
                "add".into(),
                "--scope".into(),
                "user".into(),
                "farhand".into(),
                "--".into(),
            ];
            args.extend(serve.iter().cloned());
            let out = claude_cli()?.args(&args).output()?;
            if !out.status.success() {
                return Err(anyhow!(
                    "`claude mcp add` failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            println!("claude-code: registered MCP server `farhand` (user scope)");
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
            let entry = json!({ "command": serve[0], "args": serve[1..] });
            if servers["farhand"] != entry {
                servers["farhand"] = entry;
                write_json(&path, &doc)?;
                println!("claude-code: wrote MCP server to {}", path.display());
            } else {
                println!("claude-code: {} already up to date", path.display());
            }
        }
    }

    let path = claude_settings_path(opts);
    let static_deny = opts.scope == Scope::Project;
    let mut settings = read_json(&path)?;
    if apply_claude_settings(&mut settings, static_deny, opts)? {
        write_json(&path, &settings)?;
        if static_deny {
            println!(
                "claude-code: denied {} and added the PreToolUse hook in {}",
                CLAUDE_LOCAL_TOOLS.join(", "),
                path.display()
            );
        } else {
            println!(
                "claude-code: added the PreToolUse hook in {} (it refuses local tools wherever \
                 FarHand is active; set activation = \"project\" in the global config to keep \
                 other directories local)",
                path.display()
            );
        }
    } else {
        println!("claude-code: {} already up to date", path.display());
    }
    println!("claude-code: approval follows [approval] in the FarHand config at call time");
    Ok(())
}

fn uninstall_claude(opts: &Options) -> Result<()> {
    match opts.scope {
        Scope::User => {
            if claude_mcp_registered()? {
                let out = claude_cli()?
                    .args(["mcp", "remove", "--scope", "user", "farhand"])
                    .output()?;
                if out.status.success() {
                    println!("claude-code: removed MCP server `farhand`");
                } else {
                    println!(
                        "claude-code: `claude mcp remove` failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            } else {
                println!("claude-code: MCP server not registered");
            }
        }
        Scope::Project => {
            let path = opts.project.join(".mcp.json");
            let mut doc = read_json(&path)?;
            if let Some(servers) = doc.get_mut("mcpServers").and_then(Value::as_object_mut) {
                if servers.remove("farhand").is_some() {
                    write_json(&path, &doc)?;
                    println!("claude-code: removed farhand from {}", path.display());
                }
            }
        }
    }
    let path = claude_settings_path(opts);
    if path.exists() {
        let mut settings = read_json(&path)?;
        if strip_claude_settings(&mut settings) {
            write_json(&path, &settings)?;
            println!(
                "claude-code: removed the {} deny entries and the hook from {}",
                CLAUDE_LOCAL_TOOLS.join(", "),
                path.display()
            );
        }
    }
    Ok(())
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
// per tool — enough for a real FarHand session at project scope. There is no
// per-call hook, so `[approval]` is translated when you run `install`; run
// it again after changing it.

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

fn install_codex(opts: &Options) -> Result<()> {
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
        println!("codex: {} already up to date", path.display());
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, after)?;
        println!("codex: wrote [mcp_servers.farhand] to {}", path.display());
    }
    match opts.scope {
        Scope::Project => println!(
            "codex: local shell off and sandbox read-only for this project; Codex loads \
             .codex/config.toml only after you trust the project"
        ),
        Scope::User => println!(
            "codex: user scope registers the server only; Codex keeps its local shell. Use \
             --scope project in a directory to take it away there."
        ),
    }
    println!("codex: [approval] was translated now; rerun install after changing it");
    Ok(())
}

fn uninstall_codex(opts: &Options) -> Result<()> {
    let path = codex_config_path(opts);
    if !path.exists() {
        println!("codex: nothing installed");
        return Ok(());
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
    if after != before {
        std::fs::write(&path, after)?;
        println!("codex: removed FarHand's entries from {}", path.display());
    } else {
        println!("codex: nothing installed");
    }
    Ok(())
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
    Ok(vec![format!(
        "mcp server: {}, local shell off: {shell_off} ({})",
        if present {
            "registered"
        } else {
            "not registered"
        },
        path.display()
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            scope: Scope::Project,
            config: None,
            project: PathBuf::from("/tmp/p"),
        }
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
