# Using FarHand

## 1. One-time setup on this machine

```
curl -fsSL https://raw.githubusercontent.com/CogFlux/farhand/main/install.sh | sh
#   or: npm i -g farhand   /   cargo binstall farhand   /   cargo install farhand
mkdir -p ~/FarHand-Outbox                      # the folder the model may upload from
farhand install opencode                       # and/or claude-code, codex
```

Nothing else is global. Which projects go remote is decided per project in
step 3; until a project has its `.farhand.toml`, FarHand is inactive there
and the agent works locally as before.

## 2. On the remote (once per host)

Nothing to install for the basics; `bash`, `timeout`, `base64` and the SFTP
subsystem are standard. Install `ripgrep` for fast `remote_glob` /
`remote_grep` (`apt install ripgrep`); FarHand falls back to `find`/`grep`
without it.

## 3. Making a project remote

In the local project's root:

```
farhand init > .farhand.toml     # then edit it
farhand check                    # connects once over your normal SSH setup
```

The three lines that matter:

```toml
[remote]
host = "devbox"            # alias from ~/.ssh/config, or user@host
workdir = "~/myapp"        # where commands start; relative paths resolve here

[local]
allowed_dirs = ["~/FarHand-Outbox", "/Users/me/Documents/myapp/assets"]
```

`farhand check` prints the instructions the model will receive. If
`ssh <host>` works in a terminal, this works.

The OpenCode plugin picks it up automatically (`--config <worktree>/.farhand.toml`).
Other agents pass it with `farhand serve --config /path/to/.farhand.toml` or
`FARHAND_CONFIG=...`. Keep `.farhand.toml` out of git (the repo's
`.gitignore` already does).

### The optional global config and `activation`

A directory without `.farhand.toml` is an ordinary local session: the
OpenCode plugin does nothing there, the Claude Code hook makes no decision,
and `farhand serve` offers zero tools and says why in its instructions.
That holds whether or not `~/.config/farhand/config.toml` exists.

The global file is for one thing: making the *whole machine* remote.

```toml
activation = "always"    # every directory is remote, using [remote] below
# activation = "project" # the default: only directories with a .farhand.toml
```

It is also a convenient target for `farhand serve --config` or
`FARHAND_CONFIG`. `farhand validate --cwd DIR` reports `active here` for any
directory.

## 4. Daily use

Start the agent in any local directory. What the model gets:

| It wants to | It calls | You see in the audit log |
|---|---|---|
| run a command | `remote_bash` | command, exit code, duration |
| read / write / edit a file | `remote_read` / `remote_write` / `remote_edit` | path, bytes |
| find files / search text | `remote_glob` / `remote_grep` | pattern |
| look at your local outbox | `local_ls` / `local_read` | path |
| move a file to the remote | `upload` | local path, remote path, bytes |
| bring a file back | `download` | remote path, local path, bytes |

Workflow for "use this local file on the server": drop it into
`~/FarHand-Outbox/`, then tell the agent "upload `foo.pdf` to `data/`". Every
file passes the secret guard first; a refusal names the rule.

Audit log: `~/.local/share/farhand/audit/YYYY-MM-DD.jsonl`. Read it with
`jq`:

```
jq -r '[.ts, .tool, .outcome, (.command // .path // "")] | @tsv' ~/.local/share/farhand/audit/$(date -u +%F).jsonl
```

Multiple hosts: one config per host, choose with `--config`. Per-tool host
selection is on the roadmap.

## 5. Agent configuration

One command per agent. Each one writes exactly what that agent needs, is
idempotent, and has a matching `uninstall`. `farhand status` shows where
things stand.

```
farhand install opencode
farhand install claude-code [--scope user|project]
farhand install codex
farhand status
```

The launch command written into the agent's config uses the absolute path of
the `farhand` binary you ran, so PATH differences between shells and GUI
launchers do not matter.

`--config PATH` pins that agent to one FarHand config regardless of
directory: the path is baked into the server command, the Claude Code hook
command and the OpenCode plugin, and an explicit config always counts as
active. Use it when a project cannot carry a `.farhand.toml`, or to bind one
agent to one remote without a global `activation = "always"`. Without it
(the normal case) the config is resolved per directory.

| | OpenCode | Claude Code | Codex |
|---|---|---|---|
| registers the MCP server | plugin | `claude mcp add` (user) / `.mcp.json` (project) | `[mcp_servers.farhand]` |
| closes local tools | plugin `config` hook | `permissions.deny` (project scope) | `features.shell_tool = false` + `sandbox_mode = "read-only"` (project scope) |
| second fence | plugin `tool.execute.before` | `PreToolUse` hook → `farhand hook claude-code` | none |
| `[approval]` | translated at startup | answered by the hook per call | `tools.<t>.approval_mode`, translated at install |
| honours `activation` | yes | yes (hook) | serve offers no tools when inactive |
| tools appear as | `farhand_remote_bash` | `mcp__farhand__remote_bash` | `farhand.remote_bash` |

**User scope vs project scope.** User scope (`--scope user`, the default)
never writes a static "local tools off" rule; it relies on the dynamic
pieces (plugin, hook) and on `activation`, so with the default
`activation = "project"` only directories with a `.farhand.toml` change.
Project scope writes the static rules as well, since it is confined to one
directory.

### OpenCode (tested)

`farhand install opencode` writes the plugin to
`~/.config/opencode/plugins/farhand.ts` with the binary path baked in. If
that path is already a symlink (a development checkout), it is left alone.
Restart OpenCode.

The plugin registers the MCP server, disables `bash`, `read`, `write`,
`edit`, `multiedit`, `patch`, `glob`, `grep`, `list` and the LSP tools,
refuses them again at call time, translates `[approval]` into OpenCode's
`permission` rules (a `farhand*` key you set yourself in `opencode.jsonc`
wins), and tells the model in the system prompt that it is working remotely.
It runs `farhand validate` first: a config that does not parse shows a toast
with the reason, registers nothing, and still keeps the local tools off, so
a session never silently falls back to local execution. `opencode run`
(non-interactive) auto-rejects anything set to ask.

To use OpenCode locally again, `farhand uninstall opencode`, or install into
one project's `.opencode/plugins/` by hand instead of the global directory.

### Claude Code (tested at project scope)

```
farhand install claude-code                    # user scope: every project
farhand install claude-code --scope project    # this project only
```

User scope runs `claude mcp add --scope user farhand -- <farhand> serve` and
adds one `PreToolUse` hook group to `~/.claude/settings.json` that runs
`farhand hook claude-code` for `Bash`, `Edit`, `Write`, `MultiEdit`,
`NotebookEdit`, `Read`, `Glob`, `Grep` and `mcp__farhand__.*`. Project scope
writes `.mcp.json` and `.claude/settings.json` in the current directory, with
the same hook plus a static `permissions.deny` for those local tools.
`farhand hook claude-code` is the subcommand that hook entry runs; there is
nothing to install separately.

The hook denies a local tool with a message naming the FarHand replacement,
and answers `mcp__farhand__<tool>` from the `[approval]` section of the
FarHand config that applies to the hook's working directory (a
`.farhand.toml` there, else the usual resolution): `allow` for `auto`, `ask`
for `ask`; `strict` makes reads ask too. The config is read on every call, so
changes take effect without reinstalling. `claude -p` (non-interactive)
cannot answer a prompt, so use `mode = "auto"` there.

With the default `activation = "project"`, a user-scope install changes
nothing in directories without a `.farhand.toml`: the hook refuses nothing
and the server offers nothing. Set `activation = "always"` in the global
config to make every Claude Code session on the machine remote. Claude Code
also asks you to approve a project `.mcp.json` the first time.

The server's own MCP instructions tell the model it is working remotely;
nothing is added to `CLAUDE.md`.

### Codex CLI

```
farhand install codex                    # user: registers the server, Codex keeps its shell
farhand install codex --scope project    # this project: local shell off, sandbox read-only
```

Codex (2026) can switch its own shell tool off (`features.shell_tool = false`,
`features.unified_exec = false`), sandbox the rest (`sandbox_mode =
"read-only"`), and approve MCP tools per tool
(`mcp_servers.farhand.tools.<tool>.approval_mode = "auto" | "prompt"`), so a
project-scope install is a real FarHand session: the model has no local
shell, cannot write locally, and `[approval]` decides which FarHand tools
prompt. Two differences from the other agents: Codex has no per-call hook,
so `[approval]` is translated when you run `install` (rerun it after changing
it), and Codex loads a project's `.codex/config.toml` only once you have
trusted that project. `activation` still applies: in a directory without
`.farhand.toml`, the server lists no tools.

If you would rather run Codex itself on the server (`ssh` + `tmux`), that
works without FarHand, but your ChatGPT/API credentials then live on the
server — the thing FarHand exists to avoid.

### Anything else that speaks MCP (Cursor, Claude Desktop, Gemini CLI, ...)

The generic stdio entry:

```json
{ "mcpServers": { "farhand": { "command": "/path/to/farhand", "args": ["serve", "--config", "/path/to/config.toml"] } } }
```

Whether the host's own local tools can be turned off is up to that host.

## 6. Turning it off

`farhand uninstall <agent>` removes what `install` added and nothing else
(one caveat: a local-tool deny entry you had written into Claude Code's
settings yourself is indistinguishable from FarHand's and goes too). No
daemon runs; the SSH master closes when the agent exits.
