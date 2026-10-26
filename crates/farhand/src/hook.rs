//! `farhand hook claude-code` — the PreToolUse hook Claude Code runs before
//! every tool call once `farhand install claude-code` has wired it in.
//! `farhand hook codex` is Codex's counterpart; see [`codex_respond`].
//!
//! Two jobs, same as the OpenCode plugin's call-time fence:
//! - a local tool (`Bash`, `Edit`, `Read`, ...) is denied with a message
//!   that names the FarHand tool to use instead;
//! - a `mcp__farhand__<tool>` call is answered from the `[approval]`
//!   section of the FarHand config that applies to the hook's `cwd`:
//!   `allow` for `auto`, `ask` for `ask`. Anything else gets no decision.
//!
//! The config is read on every call, so changing `[approval]` takes effect
//! immediately, without reinstalling. The hook's `cwd` follows the session
//! into subfolders and worktrees, so the project file is looked for above
//! it too, and failing that in `$CLAUDE_PROJECT_DIR`, where the session
//! started. When the config that applies is active for neither (global
//! `activation = "project"` and no `.farhand.toml`), the hook makes no
//! decision at all and the session stays local.

use std::io::Read;
use std::path::{Path, PathBuf};

use farhand_core::config::ApprovalMode;
use farhand_core::Config;
use serde::Deserialize;

/// Claude Code's tools that touch the local machine. `Monitor` runs a
/// local command and streams its output, `PowerShell` is the shell tool on
/// Windows, and `EnterWorktree` creates a local git worktree and moves the
/// session into it.
pub const CLAUDE_LOCAL_TOOLS: &[&str] = &[
    "Bash",
    "PowerShell",
    "Monitor",
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "Read",
    "Glob",
    "Grep",
    "EnterWorktree",
];

pub const MCP_PREFIX: &str = "mcp__farhand__";

#[derive(Deserialize)]
struct HookInput {
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    tool_name: String,
}

fn decision(kind: &str, reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": kind,
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

/// Which FarHand tool replaces a given Claude Code local tool.
fn replacement(tool: &str) -> &'static str {
    match tool {
        "Bash" | "PowerShell" => "mcp__farhand__remote_shell",
        "Monitor" => "mcp__farhand__remote_spawn",
        "Edit" | "MultiEdit" | "NotebookEdit" => "mcp__farhand__remote_edit",
        "Write" => "mcp__farhand__remote_write",
        "Read" => "mcp__farhand__remote_read",
        "Glob" => "mcp__farhand__remote_glob",
        "Grep" => "mcp__farhand__remote_grep",
        _ => "the mcp__farhand__* tools",
    }
}

/// Compute the hook's stdout for one input. `None` means "no decision".
/// `project_dir` is the session's starting folder, consulted when `cwd`
/// finds no active config.
pub fn respond(
    input_json: &str,
    explicit_config: Option<&Path>,
    project_dir: Option<&Path>,
) -> Option<String> {
    let input: HookInput = serde_json::from_str(input_json).ok()?;
    let tool = input.tool_name.as_str();
    let is_local = CLAUDE_LOCAL_TOOLS.contains(&tool);
    let farhand_tool = tool.strip_prefix(MCP_PREFIX);
    if !is_local && farhand_tool.is_none() {
        return None;
    }

    let mut loaded = Config::load_in(input.cwd.as_deref(), explicit_config);
    let found_nothing = match &loaded {
        Ok(l) => !l.active(),
        Err(e) => matches!(e, farhand_core::Error::NoConfig),
    };
    if found_nothing {
        if let Some(dir) = project_dir {
            loaded = Config::load_in(Some(dir), explicit_config);
        }
    }
    let config = match loaded {
        Ok(l) if !l.active() => return None,
        Ok(l) => l.config,
        // No config anywhere: not a FarHand session, nothing to say.
        Err(farhand_core::Error::NoConfig) => return None,
        // A config that exists but does not parse: ask, never assume.
        Err(e) => {
            return Some(decision(
                "ask",
                &format!("FarHand: cannot read its config ({e}); asking to be safe"),
            ))
        }
    };

    if is_local {
        return Some(decision(
            "deny",
            &format!(
                "FarHand: the local tool {tool} is disabled in this session; everything runs on \
                 the remote host. Use {} instead.",
                replacement(tool)
            ),
        ));
    }

    let farhand_tool = farhand_tool?;
    match config.approval.effective().get(farhand_tool).copied() {
        Some(ApprovalMode::Auto) => Some(decision(
            "allow",
            &format!("FarHand [approval]: {farhand_tool} is set to auto"),
        )),
        Some(ApprovalMode::Ask) | Some(ApprovalMode::Strict) => Some(decision(
            "ask",
            &format!("FarHand [approval]: {farhand_tool} is set to ask"),
        )),
        None => None,
    }
}

pub fn run_claude(explicit_config: Option<&Path>) -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let project_dir = std::env::var_os("CLAUDE_PROJECT_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    if let Some(out) = respond(&input, explicit_config, project_dir.as_deref()) {
        println!("{out}");
    }
    Ok(())
}

/// Codex's tools that touch the local machine, by the names its hooks
/// see: `Bash` is `exec_command` (and the shell tool), `apply_patch` writes
/// files, `view_image` reads a local image into the conversation.
pub const CODEX_LOCAL_TOOLS: &[&str] = &["Bash", "apply_patch", "view_image"];

#[derive(Deserialize)]
struct CodexHookInput {
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    hook_event_name: String,
    #[serde(default)]
    tool_name: String,
}

fn codex_replacement(tool: &str) -> &'static str {
    match tool {
        "Bash" => "FarHand's remote_shell",
        "apply_patch" => "FarHand's remote_edit or remote_write",
        _ => "FarHand's remote tools (download a file to look at it locally)",
    }
}

fn codex_session_context(text: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": text,
        }
    })
    .to_string()
}

/// The hook Codex runs for `PreToolUse` and `SessionStart` once `farhand
/// install codex` has wired it in. Codex has no `ask` for hooks and lets a
/// call through when its hook fails, so this hook only ever denies: a local
/// tool wherever FarHand is active, and wherever the config does not load.
/// At session start it tells the model, where FarHand is active, that the
/// work is remote. `[approval]` stays with Codex's per-tool approval modes.
pub fn codex_respond(input_json: &str, explicit_config: Option<&Path>) -> Option<String> {
    let input: CodexHookInput = serde_json::from_str(input_json).ok()?;
    let local_tool = match input.hook_event_name.as_str() {
        "PreToolUse" if CODEX_LOCAL_TOOLS.contains(&input.tool_name.as_str()) => {
            Some(input.tool_name.as_str())
        }
        "SessionStart" => None,
        _ => return None,
    };

    let config = match Config::load_in(input.cwd.as_deref(), explicit_config) {
        Ok(l) if !l.active() => return None,
        Ok(l) => l.config,
        Err(farhand_core::Error::NoConfig) => return None,
        Err(e) => {
            return Some(match local_tool {
                Some(tool) => decision(
                    "deny",
                    &format!(
                        "FarHand: cannot read its config ({e}); the local tool {tool} stays \
                         off until it is fixed (run `farhand check`)"
                    ),
                ),
                None => codex_session_context(&format!(
                    "FarHand is configured for this directory but its config does not load \
                     ({e}). Do no file or shell work; tell the user to fix it (`farhand \
                     check`)."
                )),
            })
        }
    };

    Some(match local_tool {
        Some(tool) => decision(
            "deny",
            &format!(
                "FarHand: the local tool {tool} is disabled in this session; everything runs \
                 on the remote host. Use {} instead.",
                codex_replacement(tool)
            ),
        ),
        None => codex_session_context(&format!(
            "FarHand is active: this session works on the REMOTE host `{host}` in `{workdir}` \
             over SSH, not on this machine. The working directory Codex reports is the local \
             one; \"here\", \"this project\" and every relative path mean the remote workdir. \
             Use FarHand's remote_* tools for every command and file operation; the local \
             shell, apply_patch and view_image are refused. Use local_ls / local_read only when \
             the user asks about their local folders, and upload / download to move files. A \
             service started on the remote is not at localhost for the user: call remote_info \
             for its address.",
            host = config.remote.host,
            workdir = config.remote.workdir,
        )),
    })
}

pub fn run_codex(explicit_config: Option<&Path>) -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    if let Some(out) = codex_respond(&input, explicit_config) {
        println!("{out}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_dir(approval: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".farhand.toml"),
            format!("[remote]\nhost='h'\nworkdir='/'\n[approval]\n{approval}\n"),
        )
        .unwrap();
        d
    }

    fn call(cwd: &Path, tool: &str) -> Option<serde_json::Value> {
        let input = serde_json::json!({ "cwd": cwd, "tool_name": tool, "tool_input": {} });
        respond(&input.to_string(), None, None).map(|s| serde_json::from_str(&s).unwrap())
    }

    fn kind(v: &Option<serde_json::Value>) -> String {
        v.as_ref().unwrap()["hookSpecificOutput"]["permissionDecision"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn local_tools_are_denied() {
        let d = cfg_dir("mode='auto'");
        for t in CLAUDE_LOCAL_TOOLS {
            let v = call(d.path(), t);
            assert_eq!(kind(&v), "deny", "{t}");
        }
        let v = call(d.path(), "Bash");
        assert!(v.unwrap()["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("mcp__farhand__remote_shell"));
    }

    #[test]
    fn subfolders_and_worktrees_of_a_project_are_fenced() {
        let d = cfg_dir("mode='ask'");
        let worktree = d.path().join(".claude/worktrees/w1/src");
        std::fs::create_dir_all(&worktree).unwrap();
        assert_eq!(kind(&call(&worktree, "Bash")), "deny");
        assert_eq!(kind(&call(&worktree, "Monitor")), "deny");
    }

    #[test]
    fn project_dir_covers_a_worktree_outside_the_project() {
        let proj = cfg_dir("mode='ask'");
        let elsewhere = tempfile::tempdir().unwrap();
        // Keep a real global config out of the picture.
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", home.path());
        let input = serde_json::json!({ "cwd": elsewhere.path(), "tool_name": "Write" });
        let with = respond(&input.to_string(), None, Some(proj.path()));
        let without = respond(&input.to_string(), None, None);
        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        assert_eq!(
            kind(&with.map(|s| serde_json::from_str(&s).unwrap())),
            "deny"
        );
        assert!(without.is_none());
    }

    #[test]
    fn farhand_tools_follow_approval() {
        // `auto` for a mutating tool comes from a config the user chose,
        // never from a project file.
        let d = tempfile::tempdir().unwrap();
        let explicit = d.path().join("fh.toml");
        std::fs::write(
            &explicit,
            "[remote]\nhost='h'\nworkdir='/'\n[approval]\nmode='ask'\ntools={ remote_write='auto' }\n",
        )
        .unwrap();
        let call_with = |tool: &str| {
            let input = serde_json::json!({ "cwd": d.path(), "tool_name": tool, "tool_input": {} });
            respond(&input.to_string(), Some(&explicit), None)
                .map(|s| serde_json::from_str(&s).unwrap())
        };
        assert_eq!(kind(&call_with("mcp__farhand__remote_shell")), "ask");
        assert_eq!(kind(&call_with("mcp__farhand__remote_write")), "allow");
        assert_eq!(kind(&call_with("mcp__farhand__remote_read")), "allow");
        let strict = cfg_dir("mode='strict'");
        assert_eq!(
            kind(&call(strict.path(), "mcp__farhand__remote_read")),
            "ask"
        );
    }

    #[test]
    fn other_tools_get_no_decision() {
        let d = cfg_dir("mode='auto'");
        assert!(call(d.path(), "WebFetch").is_none());
        assert!(call(d.path(), "mcp__other__thing").is_none());
        assert!(call(d.path(), "mcp__farhand__unknown").is_none());
    }

    #[test]
    fn inactive_config_makes_no_decision() {
        // A global config with activation = "project" reached through the
        // explicit path counts as explicit (active); only the real global
        // file can be inactive, so emulate that via Loaded directly.
        use farhand_core::config::{Loaded, Source};
        let cfg =
            farhand_core::Config::parse("activation='project'\n[remote]\nhost='h'\nworkdir='/'")
                .unwrap();
        let l = Loaded {
            config: cfg,
            path: PathBuf::new(),
            source: Source::Global,
            notices: Vec::new(),
        };
        assert!(!l.active());
    }

    #[test]
    fn no_config_makes_no_decision() {
        let d = tempfile::tempdir().unwrap();
        // Route the "global" lookup somewhere empty too.
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", home.path());
        let r = call(d.path(), "Bash");
        let r2 = call(d.path(), "mcp__farhand__remote_shell");
        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        assert!(r.is_none());
        assert!(r2.is_none());
    }

    fn codex_call(cwd: &Path, event: &str, tool: &str) -> Option<serde_json::Value> {
        let input = serde_json::json!({
            "cwd": cwd, "hook_event_name": event, "tool_name": tool, "tool_input": {}
        });
        codex_respond(&input.to_string(), None).map(|s| serde_json::from_str(&s).unwrap())
    }

    #[test]
    fn codex_local_tools_are_denied_and_others_left_alone() {
        let d = cfg_dir("mode='ask'");
        for t in CODEX_LOCAL_TOOLS {
            assert_eq!(kind(&codex_call(d.path(), "PreToolUse", t)), "deny", "{t}");
        }
        // Codex has no `ask`: FarHand's own tools get no decision at all.
        assert!(codex_call(d.path(), "PreToolUse", "mcp__farhand__remote_shell").is_none());
        assert!(codex_call(d.path(), "PostToolUse", "Bash").is_none());
        let sub = d.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(kind(&codex_call(&sub, "PreToolUse", "Bash")), "deny");
    }

    #[test]
    fn codex_session_start_says_the_work_is_remote() {
        let d = cfg_dir("mode='ask'");
        let v = codex_call(d.path(), "SessionStart", "").unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        let text = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(text.contains("REMOTE host `h`"), "{text}");
    }

    #[test]
    fn codex_broken_config_denies_instead_of_asking() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".farhand.toml"), "nonsense = [").unwrap();
        assert_eq!(kind(&codex_call(d.path(), "PreToolUse", "Bash")), "deny");
        let v = codex_call(d.path(), "SessionStart", "").unwrap();
        assert!(v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("does not load"));
    }

    #[test]
    fn broken_config_asks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".farhand.toml"), "nonsense = [").unwrap();
        assert_eq!(kind(&call(d.path(), "mcp__farhand__remote_shell")), "ask");
    }
}
