# FarHand

Gives a coding agent hands on a remote machine and keeps its hands off the
local one. **https://farhand.cogflux.io**

FarHand is an MCP server. Point OpenCode, Claude Code or Codex at it, and every
command the model runs and every file it reads or writes happens on a remote
host over your existing SSH setup. The local machine stays closed, with one
exception you control: a list of local folders the model may look into and
upload from. Nothing that looks like a credential is ever sent to the remote,
and everything that happens is written to a local audit log.

```
┌─ OpenCode ─┐   ┌─ Claude Code ─┐   ┌─ CogFlux (later) ─┐
│ thin plugin │   │ .mcp.json     │   │ --mcp-config      │
└──────┬──────┘   └──────┬────────┘   └─────────┬─────────┘
       └─────────────────┴── MCP over stdio ────┘
                       farhand (one binary)
        ┌──────────────────┼───────────────────┐
   remote tools       local tools          audit log
   remote_shell        local_ls / local_read     one JSON line per action
   remote_read/write  upload / download
   remote_edit/ls     (allowlisted folders only)
   remote_glob/grep
        │
   your OpenSSH: ~/.ssh/config, keys, agent, jump hosts — one multiplexed session
```

## Why not `opencode serve` on the remote?

That works, but the agent's provider keys and configuration have to live on
the remote, and every agent needs its own way of doing it. FarHand keeps the
agent — and its credentials — local, and gives every MCP-capable agent the
same remote hands.

## Install

Local: macOS or Linux with OpenSSH. Remote: Linux with `bash`, `timeout`
(coreutils), `base64`, an SFTP subsystem (default in sshd), and ideally
`ripgrep` for `remote_glob` / `remote_grep` (both fall back to `find` /
`grep`). Windows is not supported: FarHand relies on OpenSSH connection
multiplexing over a Unix socket.

**Prebuilt binary** (no Rust needed) — one file into `~/.local/bin`, checksum
verified:

```
curl -fsSL https://raw.githubusercontent.com/CogFlux/farhand/main/install.sh | sh
```

Or download `farhand-<version>-<target>.tar.gz` from the
[releases page](https://github.com/CogFlux/farhand/releases) yourself, or
`cargo binstall farhand`.

**From source** (Rust stable): `cargo install --path crates/farhand`
puts `farhand` in `~/.cargo/bin`; or `cargo build --release` and point
`FARHAND_BIN` at `target/release/farhand`.

Then, either way:

```
farhand install opencode                  # and/or claude-code, codex — once per machine
cd ~/my-project
farhand init > .farhand.toml              # edit host, workdir, allowed_dirs
farhand check                             # connects once and reports
```

That is all: a project is remote exactly when it has a `.farhand.toml`.
There is no global configuration to write unless you want one (see
Configure).

See `docs/USAGE.md` for the step-by-step guide and per-agent configuration.

## Configure

`farhand init` prints a commented template. The essentials:

```toml
[remote]
host = "devbox"            # an alias from ~/.ssh/config, or user@host
workdir = "~/proj"         # commands start here; relative paths resolve here; created if missing

[local]
allowed_dirs = ["."]       # the only local folders the model may see; "." = this folder
```

With `"."` the folder holding `.farhand.toml` is the project's local pocket:
drop a file in it and the model can `upload` it, and `download` lands there.
Nothing else lives in that folder — the code is on the remote — so this is
the natural default. Absolute paths and `~/...` work too; relative entries
are only accepted in a project `.farhand.toml`, never `..`, `/` or `~`.

The remote may be Linux, macOS, BSD — or **Windows** with OpenSSH Server,
where commands run in PowerShell (`os = "windows"` under `[remote]` tells
the model so up front; the platform is detected on connect regardless).
Directory creation and tree walking go over SFTP, so uploads, downloads,
`remote_write` and `remote_edit` behave the same everywhere. Transfers are
streamed with many requests in flight, so a large file moves at link speed
and memory stays flat whatever its size; the default cap of 1 GiB per
transfer is `max_transfer_bytes` under `[limits]`.

Config is found in this order: `--config PATH`, `$FARHAND_CONFIG`,
`./.farhand.toml`, `~/.config/farhand/config.toml`. The per-project
`.farhand.toml` is the normal one: it is what makes a directory remote. The
global file is optional and serves two purposes: `activation = "always"`
turns every directory on the machine remote using its `[remote]`, and it is
a convenient target for `--config`. Without any config at all, FarHand is
simply inactive: the agent keeps its local tools and the server offers none.

## Agents

`farhand install <agent>` does the wiring for `opencode`, `claude-code` and
`codex` (`--scope user|project`); `farhand uninstall` reverses it and
`farhand status` reports it. By default only directories that carry a
`.farhand.toml` go remote (`activation = "project"`); everywhere else the
agent keeps its local tools. `activation = "always"` in the global config
makes every session remote. What each agent gets is in `docs/USAGE.md`.

## OpenCode

`farhand install opencode` writes `plugins/opencode/farhand.ts` into
`~/.config/opencode/plugins/` (or symlink it there yourself for
development). The one file carries both plugin APIs, so it works with
OpenCode 1.18.29+ and OpenCode 2 alike and survives an upgrade between them.
The plugin registers the MCP server, removes OpenCode's local tools
(`bash`/`shell`, `read`, `write`, `edit`, `patch`, `glob`, `grep`, `list`,
the LSP tools, and in OpenCode 2 the `browser_*` tools and Code Mode's
`execute`, since a local browser and `execute`'s `fetch` can both open
`file://` URLs), refuses them again at call time as a second fence, and tells the model in the system prompt that it is working
remotely.

Approval is FarHand's knob, not the agent's: `[approval] mode = "ask"`
(default) makes the agent prompt before `remote_shell`, `remote_write`,
`remote_edit`, `upload` and `download` and lets read-only tools run;
`"auto"` prompts for nothing, with the audit log as the record; `"strict"`
prompts for everything, reads included; `tools = { remote_write = "auto" }`
overrides per tool. A project `.farhand.toml` can make this stricter but not
looser than the global config; `auto` belongs in the global file. The
plugin translates this into OpenCode's permission system at startup.

Without the plugin, the same effect comes from `opencode.jsonc` (OpenCode 1
format shown; OpenCode 2 reads it too, but cannot remove `browser_*` from
config alone — use the plugin there):

```jsonc
{
  "mcp": {
    "farhand": { "type": "local", "command": ["farhand", "serve"], "enabled": true, "timeout": 60000 }
  },
  "tools": { "bash": false, "read": false, "write": false, "edit": false,
             "glob": false, "grep": false, "list": false, "patch": false },
  "permission": { "farhand_remote_shell": "ask", "farhand_remote_write": "ask",
                  "farhand_remote_edit": "ask", "farhand_upload": "ask", "farhand_download": "ask" }
}
```

The tools then appear as `farhand_remote_shell`, `farhand_upload`, and so on.

## Tools

| Tool | What it does |
|---|---|
| `remote_shell` | Run a command on the remote in its own shell — POSIX shell, or PowerShell on Windows (bounded output, remote timeout, local watchdog). |
| `remote_read` | Read a remote file with line numbers; `offset`/`limit` page through it. |
| `remote_write` | Create or overwrite a remote file; parents are created. |
| `remote_edit` | Replace an exact, unique string in a remote file (`replace_all` optional; CRLF files are matched with LF strings and kept CRLF). |
| `remote_ls` / `remote_glob` / `remote_grep` | `ls -la`, `rg --files -g`, `rg -n` on the remote. |
| `local_ls` / `local_read` | Look inside the allowlisted local folders. |
| `upload` | Copy a local file or folder (allowlist only, guard-checked) to the remote. |
| `download` | Copy a remote file or folder into an allowlisted local folder (never credential-shaped names or files a local tool would execute). |
| `remote_info` | Host, workdir, connection state, allowed folders, audit location. |

## What never leaves this machine

Every byte headed for the remote — command text, file content, edits,
uploads, and every other argument (paths, `cwd`, search patterns and
globs) — passes the secret guard first:

- **by path** (case-insensitively): `.env*`, `*.pem`, `*.key`, `id_*`, `.netrc`, `.npmrc`,
  `credentials*`, `*service-account*.json`, `*.tfstate`, agent configs
  (`opencode.json*`, `.mcp.json`, `claude_desktop_config.json`, ...), and
  anything under `.ssh`, `.aws`, `.gnupg`, `.kube`, `.docker`, `.config`,
  `.claude`, `.opencode`, `.codex`, ...;
- **by content**: private-key blocks, `sk-…`, `ghp_…` / `github_pat_…`,
  `AKIA…`, `AIza…`, `xox…`, Stripe, JWTs, `Authorization: Bearer …`, URLs
  with embedded passwords, and `api_key = "<24+ random chars>"` style
  assignments.

Add your own with `[guard] deny_globs` / `deny_content`. A refusal comes back
to the model as a tool error naming the rule, never the matched text, and is
audited the same way. There is no override flag.

The local environment is never forwarded to the remote. Commands run in the
remote user's own login shell.

## Threat model

FarHand is built so that the model can be *wrong or hostile* and the damage
stays on the remote. What holds regardless of which model is driving:

- **No local execution.** The only process FarHand ever starts locally is
  `ssh` to the configured host (plus `ssh -G` to print its address). No
  model-supplied string reaches a local command line.
- **Local reads are the allowlist, minus credentials.** `local_ls`,
  `local_read` and `upload` resolve paths through the allowlist with
  symlinks followed first, so a link out of the folder is refused. Files the
  guard would refuse to upload are hidden from `local_ls` and refused by
  `local_read`: what the model cannot see it cannot re-encode. `/`, the
  home directory and any folder above it are not accepted as allowed
  folders.
- **Downloads cannot plant anything.** Every file a download would create is
  resolved through the allowlist again and checked by name (case-insensitively):
  nothing credential-shaped, and nothing an editor or agent executes or
  trusts on opening a folder (`.vscode`, `.idea`, `.git`, `.husky`,
  `.envrc`, `.claude`, `.farhand.toml`, `CLAUDE.md`, ...). A refused name
  aborts the whole download before the first byte is written. The data goes
  to a randomly named temporary file created exclusively and without
  following symlinks, then is renamed into place, so a link planted in the
  folder is replaced rather than written through.
- **Only hosts you have already met.** SSH host keys are checked strictly:
  a `.farhand.toml` that arrives inside a cloned repository cannot point the
  agent at a machine you never connected to. Connect once by hand first.
- **A project file can only tighten.** A `.farhand.toml` may arrive with a
  cloned repository, so it cannot loosen `[approval]` below the global
  config (or the default `ask`), allow local folders outside its own
  directory, or move the audit log. Such settings are ignored, and
  `farhand check` and `farhand validate` say which. Put them in the global
  config instead.
- **Everything is on record.** Every call, including refusals, is one line
  in the audit log. A log directory that cannot be written stops FarHand at
  startup instead of losing records quietly.

What FarHand does not do: it cannot stop a model that has legitimately read a
non-secret file in the allowlist from sending it to the remote (that is the
point of the allowlist), it does not restrict the remote itself (treat the
remote account as the model's sandbox and give it only what that account
should have), and it does not disable *other* MCP servers or tools your agent
has that reach the local machine. Keep secrets out of `allowed_dirs`; the
guard is a tripwire for accidents, not a substitute for that.

There is deliberately no local `curl`/fetch tool: `curl` reads local files
(`-d @file`, `file://`) and reaches local-only services (Docker sockets,
IDE and browser debug ports, `localhost` dashboards), and an approval prompt
is not a defence against a request that looks harmless. Fetch from the remote
with `remote_shell` instead; the remote has `curl` too.

## Audit log

`~/.local/share/farhand/audit/YYYY-MM-DD.jsonl`, one object per action:
timestamp, session, host, tool, command or path, byte count, outcome
(`ok` / `denied` / `error` / `timeout`), exit code, duration. File contents
are never logged, and a command the guard refused is logged as redacted.

## Layout

```
crates/farhand-core   config, guard, local allowlist, remote (ssh + sftp), transfer, audit
crates/farhand    the `farhand` binary: MCP tools over stdio
crates/farhand/plugins/opencode   the OpenCode entry point (embedded in the binary)
packages/npm/farhand  the npm wrapper (`npx farhand`), fetches the release binary
```

## Claude Code

`farhand install claude-code` registers the server and adds a `PreToolUse`
hook (`farhand hook claude-code`) that refuses local tools wherever FarHand
is active and answers `mcp__farhand__*` calls from `[approval]` on every
call; project scope also writes a static `permissions.deny`. Tested at
project scope.

## Codex

`farhand install codex --scope project` registers the server, turns Codex's
shell tool off, sets the sandbox read-only and translates `[approval]` into
per-tool `approval_mode`. User scope registers the server only.

## Roadmap

- CogFlux integration: launched via `--mcp-config`, audit log as an ingest
  source.
- Several remotes per config with a `host` argument on the tools.
