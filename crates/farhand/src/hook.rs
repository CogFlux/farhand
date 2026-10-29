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

/// What FarHand's Codex `PreToolUse` group matches: the local tools, and
/// FarHand's own tools for the approval check.
pub fn codex_matcher() -> String {
    format!("{}|{MCP_PREFIX}.*", CODEX_LOCAL_TOOLS.join("|"))
}

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
/// work is remote.
///
/// `[approval]` reaches Codex as per-tool approval modes written by
/// `install`, which can go stale or come from another folder's config (a
/// user-scope install serves every project). So a `mcp__farhand__<tool>`
/// call that the config here sets to `ask` is denied when the Codex config
/// in force would run it without prompting.
pub fn codex_respond(input_json: &str, explicit_config: Option<&Path>) -> Option<String> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")))?;
    codex_respond_in(
        input_json,
        explicit_config,
        &codex_home,
        dirs::home_dir().as_deref(),
    )
}

enum CodexCall<'a> {
    Local(&'a str),
    FarHand(&'a str),
    SessionStart,
}

fn codex_respond_in(
    input_json: &str,
    explicit_config: Option<&Path>,
    codex_home: &Path,
    home: Option<&Path>,
) -> Option<String> {
    let input: CodexHookInput = serde_json::from_str(input_json).ok()?;
    let call = match input.hook_event_name.as_str() {
        "PreToolUse" if CODEX_LOCAL_TOOLS.contains(&input.tool_name.as_str()) => {
            CodexCall::Local(input.tool_name.as_str())
        }
        "PreToolUse" => CodexCall::FarHand(input.tool_name.strip_prefix(MCP_PREFIX)?),
        "SessionStart" => CodexCall::SessionStart,
        _ => return None,
    };

    let config = match Config::load_in(input.cwd.as_deref(), explicit_config) {
        Ok(l) if !l.active() => return None,
        Ok(l) => l.config,
        Err(farhand_core::Error::NoConfig) => return None,
        Err(e) => {
            return Some(match call {
                CodexCall::Local(tool) => decision(
                    "deny",
                    &format!(
                        "FarHand: cannot read its config ({e}); the local tool {tool} stays \
                         off until it is fixed (run `farhand check`)"
                    ),
                ),
                CodexCall::FarHand(tool) => decision(
                    "deny",
                    &format!(
                        "FarHand: cannot read its config ({e}), so it cannot tell whether \
                         {tool} needs approval; refused until it is fixed (run `farhand check`)"
                    ),
                ),
                CodexCall::SessionStart => codex_session_context(&format!(
                    "FarHand is configured for this directory but its config does not load \
                     ({e}). Do no file or shell work; tell the user to fix it (`farhand \
                     check`)."
                )),
            });
        }
    };

    match call {
        CodexCall::Local(tool) => Some(decision(
            "deny",
            &format!(
                "FarHand: the local tool {tool} is disabled in this session; everything runs \
                 on the remote host. Use {} instead.",
                codex_replacement(tool)
            ),
        )),
        CodexCall::FarHand(tool) => {
            let wanted = config.approval.effective().get(tool).copied()?;
            if wanted == ApprovalMode::Auto {
                return None;
            }
            let cwd = input.cwd.as_deref()?;
            let installed = codex_approval_mode(cwd, codex_home, home, tool)?;
            (installed != "prompt").then(|| {
                decision(
                    "deny",
                    &format!(
                        "FarHand [approval]: {tool} is set to ask here, but Codex is set up to \
                         run it without asking (`approval_mode = \"{installed}\"`, written by \
                         `farhand install codex` from another config or before [approval] \
                         changed). Refused; tell the user to rerun `farhand install codex` \
                         (with `--scope project` for a project install)."
                    ),
                )
            })
        }
        CodexCall::SessionStart => {
            let remote = config.remote.as_ref()?;
            Some(codex_session_context(&format!(
                "FarHand is active: this session works on the REMOTE host `{host}` in \
                 `{workdir}` over SSH, not on this machine. The working directory Codex reports \
                 is the local one; \"here\", \"this project\" and every relative path mean the \
                 remote workdir. Use FarHand's remote_* tools for every command and file \
                 operation; the local shell, apply_patch and view_image are refused. Use \
                 local_ls / local_read only when the user asks about their local folders, and \
                 upload / download to move files. A service started on the remote is not at \
                 localhost for the user: call remote_info for its address.",
                host = remote.host,
                workdir = remote.workdir,
            )))
        }
    }
}

/// The approval mode Codex applies to FarHand's `tool` in `cwd`: from the
/// nearest `.codex/config.toml` above `cwd` when that project is trusted
/// and configures FarHand, else from the user's `config.toml` in
/// `codex_home`. `None` when neither configures FarHand.
fn codex_approval_mode(
    cwd: &Path,
    codex_home: &Path,
    home: Option<&Path>,
    tool: &str,
) -> Option<String> {
    let read = |p: &Path| -> Option<toml_edit::DocumentMut> {
        std::fs::read_to_string(p).ok()?.parse().ok()
    };
    let mode_in = |doc: &toml_edit::DocumentMut| -> Option<String> {
        let server = doc.get("mcp_servers")?.get("farhand")?;
        server
            .get("tools")
            .and_then(|t| t.get(tool))
            .and_then(|t| t.get("approval_mode"))
            .or_else(|| server.get("default_tools_approval_mode"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
    };
    let user = read(&codex_home.join("config.toml"));
    let project = cwd
        .ancestors()
        .take_while(|d| d.parent().is_some() && Some(*d) != home)
        .find(|d| d.join(".codex").join("config.toml").is_file());
    if let Some(dir) = project {
        // Codex reads a project's config only once the project is trusted.
        let key = dir.to_string_lossy();
        let trusted = user.as_ref().and_then(|u| {
            u.get("projects")?
                .get(key.as_ref())?
                .get("trust_level")?
                .as_str()
        }) == Some("trusted");
        if trusted {
            if let Some(m) = read(&dir.join(".codex").join("config.toml"))
                .as_ref()
                .and_then(&mode_in)
            {
                return Some(m);
            }
        }
    }
    user.as_ref().and_then(mode_in)
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
        let none = tempfile::tempdir().unwrap();
        codex_call_in(cwd, none.path(), event, tool)
    }

    fn codex_call_in(
        cwd: &Path,
        codex_home: &Path,
        event: &str,
        tool: &str,
    ) -> Option<serde_json::Value> {
        let input = serde_json::json!({
            "cwd": cwd, "hook_event_name": event, "tool_name": tool, "tool_input": {}
        });
        codex_respond_in(&input.to_string(), None, codex_home, None)
            .map(|s| serde_json::from_str(&s).unwrap())
    }

    fn codex_farhand(modes: &str) -> String {
        format!(
            "[mcp_servers.farhand]\ncommand = \"farhand\"\n\
             default_tools_approval_mode = \"prompt\"\n{modes}"
        )
    }

    #[test]
    fn codex_denies_a_farhand_tool_installed_looser_than_the_config() {
        let d = cfg_dir("mode='ask'\ntools={ remote_write='auto' }");
        let codex = tempfile::tempdir().unwrap();
        std::fs::write(
            codex.path().join("config.toml"),
            codex_farhand(
                "[mcp_servers.farhand.tools.remote_shell]\napproval_mode = \"auto\"\n\
                 [mcp_servers.farhand.tools.remote_edit]\napproval_mode = \"prompt\"\n",
            ),
        )
        .unwrap();
        let at = |tool: &str| codex_call_in(d.path(), codex.path(), "PreToolUse", tool);
        // ask here, auto in Codex: refused
        let v = at("mcp__farhand__remote_shell");
        assert_eq!(kind(&v), "deny");
        assert!(v.unwrap()["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("farhand install codex"));
        // ask here, prompt in Codex (explicit or by default): Codex asks
        assert!(at("mcp__farhand__remote_edit").is_none());
        assert!(at("mcp__farhand__upload").is_none());
        // auto here: nothing to check
        assert!(at("mcp__farhand__remote_write").is_none());
        // a read-only tool is auto under `ask`
        assert!(at("mcp__farhand__remote_read").is_none());
    }

    #[test]
    fn codex_uses_a_trusted_project_config_over_the_user_one() {
        let d = cfg_dir("mode='ask'");
        let codex = tempfile::tempdir().unwrap();
        let auto = "[mcp_servers.farhand.tools.remote_shell]\napproval_mode = \"auto\"\n";
        let prompt = "[mcp_servers.farhand.tools.remote_shell]\napproval_mode = \"prompt\"\n";
        std::fs::create_dir_all(d.path().join(".codex")).unwrap();
        std::fs::write(d.path().join(".codex/config.toml"), codex_farhand(prompt)).unwrap();
        let user = |trusted: bool| {
            let mut text = codex_farhand(auto);
            if trusted {
                text.push_str(&format!(
                    "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                    d.path().display()
                ));
            }
            std::fs::write(codex.path().join("config.toml"), text).unwrap();
        };
        let sub = d.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let shell = || {
            codex_call_in(
                &sub,
                codex.path(),
                "PreToolUse",
                "mcp__farhand__remote_shell",
            )
        };
        // Untrusted: Codex ignores the project file, so the user's auto applies.
        user(false);
        assert_eq!(kind(&shell()), "deny");
        user(true);
        assert!(shell().is_none());
    }

    #[test]
    fn codex_without_farhand_in_its_config_gets_no_approval_decision() {
        let d = cfg_dir("mode='ask'");
        let codex = tempfile::tempdir().unwrap();
        std::fs::write(codex.path().join("config.toml"), "model = \"gpt\"\n").unwrap();
        assert!(codex_call_in(
            d.path(),
            codex.path(),
            "PreToolUse",
            "mcp__farhand__remote_shell"
        )
        .is_none());
    }

    #[test]
    fn codex_local_tools_are_denied_and_others_left_alone() {
        let d = cfg_dir("mode='ask'");
        for t in CODEX_LOCAL_TOOLS {
            assert_eq!(kind(&codex_call(d.path(), "PreToolUse", t)), "deny", "{t}");
        }
        assert!(codex_call(d.path(), "PreToolUse", "mcp__other__thing").is_none());
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
        assert_eq!(
            kind(&codex_call(
                d.path(),
                "PreToolUse",
                "mcp__farhand__remote_read"
            )),
            "deny"
        );
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
