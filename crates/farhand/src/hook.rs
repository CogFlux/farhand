//! `farhand hook claude-code` — the PreToolUse hook Claude Code runs before
//! every tool call once `farhand install claude-code` has wired it in.
//!
//! Two jobs, same as the OpenCode plugin's call-time fence:
//! - a local tool (`Bash`, `Edit`, `Read`, ...) is denied with a message
//!   that names the FarHand tool to use instead;
//! - a `mcp__farhand__<tool>` call is answered from the `[approval]`
//!   section of the FarHand config that applies to the hook's `cwd`:
//!   `allow` for `auto`, `ask` for `ask`. Anything else gets no decision.
//!
//! The config is read on every call, so changing `[approval]` takes effect
//! immediately, without reinstalling. When the config that applies is not
//! active for the hook's `cwd` (global `activation = "project"` and no
//! `.farhand.toml` there), the hook makes no decision at all and the
//! session stays local.

use std::io::Read;
use std::path::{Path, PathBuf};

use farhand_core::config::ApprovalMode;
use farhand_core::Config;
use serde::Deserialize;

/// Claude Code's tools that touch the local machine.
pub const CLAUDE_LOCAL_TOOLS: &[&str] = &[
    "Bash",
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "Read",
    "Glob",
    "Grep",
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
        "Bash" => "mcp__farhand__remote_shell",
        "Edit" | "MultiEdit" | "NotebookEdit" => "mcp__farhand__remote_edit",
        "Write" => "mcp__farhand__remote_write",
        "Read" => "mcp__farhand__remote_read",
        "Glob" => "mcp__farhand__remote_glob",
        "Grep" => "mcp__farhand__remote_grep",
        _ => "the mcp__farhand__* tools",
    }
}

/// Compute the hook's stdout for one input. `None` means "no decision".
pub fn respond(input_json: &str, explicit_config: Option<&Path>) -> Option<String> {
    let input: HookInput = serde_json::from_str(input_json).ok()?;
    let tool = input.tool_name.as_str();
    let is_local = CLAUDE_LOCAL_TOOLS.contains(&tool);
    let farhand_tool = tool.strip_prefix(MCP_PREFIX);
    if !is_local && farhand_tool.is_none() {
        return None;
    }

    let loaded = Config::load_in(input.cwd.as_deref(), explicit_config);
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

pub fn run(explicit_config: Option<&Path>) -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    if let Some(out) = respond(&input, explicit_config) {
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
        respond(&input.to_string(), None).map(|s| serde_json::from_str(&s).unwrap())
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
            respond(&input.to_string(), Some(&explicit)).map(|s| serde_json::from_str(&s).unwrap())
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

    #[test]
    fn broken_config_asks() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".farhand.toml"), "nonsense = [").unwrap();
        assert_eq!(kind(&call(d.path(), "mcp__farhand__remote_shell")), "ask");
    }
}
