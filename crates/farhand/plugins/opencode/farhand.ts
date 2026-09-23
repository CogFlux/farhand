/**
 * FarHand entry point for OpenCode — one file for OpenCode 1.18.29+ (V1
 * plugin API) and OpenCode 2 (V2 plugin API).
 *
 * This file is deliberately thin. All the real work — the SSH session, the
 * secret guard, the local allowlist, the audit log — lives in the `farhand`
 * binary, which speaks MCP. The plugin does three things:
 *
 *   1. registers `farhand` as a local MCP server and turns every local
 *      shell / file / browser tool off, so the model's only hands are the
 *      remote ones;
 *   2. refuses any local tool call that slips through, as a second fence;
 *   3. tells the model, in the system prompt, where it is working.
 *
 * V1 calls the default export's `server()`; V2 reads its `id` and `setup()`.
 * A V1-only file fails to load in V2 with nothing but a log line, and the
 * session then runs locally with every tool on — so both halves live here
 * and neither version can end up with the other's plugin.
 *
 * Install: `farhand install opencode`, or copy this file into
 * `~/.config/opencode/plugins/` or a project's `.opencode/plugins/`. The
 * `farhand` binary must be on PATH, or set FARHAND_BIN. Configuration is
 * read by the binary itself (see `farhand init`); a `.farhand.toml` at the
 * project root takes precedence. Nothing is imported from OpenCode's
 * packages, so the file loads without a node_modules next to it.
 */

import { execFile } from "node:child_process"
import { existsSync } from "node:fs"
import { join } from "node:path"

/**
 * Tools that touch the local machine, across both versions. V2 renamed
 * `bash` to `shell` and dropped `list`; its `browser_*` tools drive a local
 * browser, which can open `file://` URLs and read local files. V2's Code
 * Mode `execute` runs model-written JavaScript whose `fetch` reads
 * `file://` URLs too (verified on 2.0.14).
 */
const LOCAL_TOOLS = new Set([
  "bash",
  "shell",
  "execute",
  "read",
  "write",
  "edit",
  "multiedit",
  "patch",
  "glob",
  "grep",
  "list",
  "ls",
])
const LOCAL_PREFIXES = ["lsp", "browser_"]

function isLocalTool(id: string): boolean {
  return LOCAL_TOOLS.has(id) || LOCAL_PREFIXES.some((p) => id.startsWith(p))
}

function refusal(tool: string): string {
  const remote: Record<string, string> = { bash: "remote_shell", shell: "remote_shell", list: "remote_ls", ls: "remote_ls", execute: "remote_shell" }
  const hint = remote[tool] ?? (LOCAL_TOOLS.has(tool) ? `remote_${tool}` : undefined)
  return (
    `FarHand: local tool "${tool}" is disabled in this session.` +
    (hint ? ` Use farhand_${hint} instead.` : " It reaches the local machine and has no remote counterpart.")
  )
}

/**
 * Approval comes from FarHand's own config (`[approval]` in farhand.toml):
 * `farhand validate --json` reports, per mutating tool, "ask" or "auto", and
 * this plugin translates that into OpenCode's permission system. Read-only
 * tools never prompt.
 */
type Approval = Record<string, "ask" | "auto">

/** Replaced with the binary's absolute path by `farhand install opencode`. */
const BUILT_IN_BIN = "farhand"
/** Set by `farhand install opencode --config PATH`: one fixed config for every session. */
const BUILT_IN_CONFIG: string | undefined = undefined

/** Longest command FarHand allows when the binary does not say (its default). */
const DEFAULT_MAX_COMMAND_SECS = 1800

const SYSTEM_NOTICE = [
  "FarHand is active: this session operates on a REMOTE host over SSH.",
  "\"The current directory\", \"here\", \"this project\" and every relative path mean the remote workdir, not this machine.",
  "The user sits at the local machine: a service started on the remote is NOT at localhost for them —",
  "report the remote's address (see farhand_remote_info) or an `ssh -L` tunnel command.",
  "The local shell, file and browser tools are disabled and will fail if called,",
  "and so is `execute` (Code Mode): call every MCP tool, farhand_* included, directly by its own name.",
  "Use the farhand_remote_* tools for every command and file operation (farhand_remote_ls to list files).",
  "Use farhand_local_ls / farhand_local_read only when the user explicitly asks about their local folders,",
  "and farhand_upload / farhand_download to move files between them and the remote.",
  "Call farhand_remote_info once if you need to know the host, workdir or allowed local folders.",
  "Credential-like content is refused by the server; never try to work around a refusal.",
].join(" ")

function problemNotice(problem: string): string {
  return (
    `FarHand is configured for this session but could not start: ${problem}. ` +
    "Local shell, file and browser tools and `execute` are disabled and no remote tools are available. " +
    "Tell the user to fix the FarHand configuration (run `farhand check`) and do not attempt any file or shell work."
  )
}

interface Session {
  /** Config did not parse or the binary could not run; tools stay off, nothing is registered. */
  problem?: string
  /** False when no FarHand config applies here: the plugin does nothing at all. */
  active: boolean
  approval: Approval
  /** The MCP server command line. */
  command: string[]
  maxCommandSecs: number
}

function run(bin: string, args: string[]): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    execFile(bin, args, { timeout: 30_000 }, (error, stdout, stderr) => {
      const code = error ? (typeof error.code === "number" ? error.code : -1) : 0
      if (error && typeof error.code !== "number") return reject(error)
      resolve({ code, stdout: String(stdout), stderr: String(stderr) })
    })
  })
}

/**
 * Parse the config up front (no network) so a bad file shows up as a
 * readable message instead of a bare MCP failure. The local tools stay off
 * either way: a misconfigured remote must not silently turn into a local
 * session.
 */
async function prepare(dirs: (string | undefined)[], options: Record<string, unknown> | undefined): Promise<Session> {
  const bin = (options?.bin as string | undefined) ?? process.env.FARHAND_BIN ?? BUILT_IN_BIN
  const roots = dirs.filter((d): d is string => !!d)
  const cwd = roots[0] ?? process.cwd()
  const projectConfig = roots.map((d) => join(d, ".farhand.toml")).find((p) => existsSync(p))
  const configPath = (options?.config as string | undefined) ?? BUILT_IN_CONFIG ?? projectConfig
  const configArgs = configPath ? ["--config", configPath] : []
  const session: Session = {
    active: true,
    approval: {},
    command: [bin, "serve", ...configArgs, "--cwd", cwd],
    maxCommandSecs: DEFAULT_MAX_COMMAND_SECS,
  }
  try {
    const result = await run(bin, ["validate", "--json", ...configArgs, "--cwd", cwd])
    const stdout = result.stdout.trim()
    let parsed: {
      ok?: boolean
      code?: string
      error?: string
      active?: boolean
      approval?: Approval
      max_command_timeout_secs?: number
    } = {}
    try {
      parsed = JSON.parse(stdout)
    } catch {
      /* not JSON: an old binary or a crash; fall through to the exit code */
    }
    if (parsed.code === "no-config") {
      // Nothing configured anywhere: not a FarHand session.
      session.active = false
    } else if (result.code !== 0 || !parsed.ok) {
      session.problem = parsed.error ?? ((result.stderr || stdout).trim() || `exit ${result.code}`)
    } else {
      session.approval = parsed.approval ?? {}
      session.active = parsed.active !== false
      session.maxCommandSecs = parsed.max_command_timeout_secs ?? DEFAULT_MAX_COMMAND_SECS
    }
  } catch (e) {
    session.problem = `cannot run \`${bin}\`: ${e instanceof Error ? e.message : String(e)}`
  }
  return session
}

// ---- OpenCode 1.x -----------------------------------------------------------

async function serverV1(input: any, options?: Record<string, unknown>) {
  const { worktree, directory, client } = input
  const s = await prepare([worktree, directory], options)
  if (s.problem) {
    client?.tui
      ?.showToast({
        body: { title: "FarHand not started", message: s.problem, variant: "error", duration: 15_000 },
      })
      .catch(() => {})
  }
  // Not active here (global `activation = "project"`, no .farhand.toml):
  // an ordinary local session, and this plugin does nothing at all.
  if (!s.problem && !s.active) return {}

  return {
    config: async (config: any) => {
      if (!s.problem) {
        config.mcp = {
          ...config.mcp,
          farhand: {
            type: "local",
            command: s.command,
            enabled: true,
            // Tool discovery waits for the SSH preflight; give slow hosts room.
            timeout: 60_000,
          },
        }
      }
      const disabled = Object.fromEntries(
        [...LOCAL_TOOLS, "lsp_diagnostics", "lsp_hover"].map((t) => [t, false]),
      )
      config.tools = { ...config.tools, ...disabled }

      // Any `farhand*` key the user has put in their own `permission`
      // config wins over the translated values.
      const permission = (config.permission ?? {}) as Record<string, unknown>
      for (const [tool, mode] of Object.entries(s.approval)) {
        const key = `farhand_${tool}`
        if (key in permission) continue
        permission[key] = mode === "auto" ? "allow" : "ask"
      }
      config.permission = permission
    },

    "tool.execute.before": async (event: { tool: string }) => {
      if (isLocalTool(event.tool)) throw new Error(refusal(event.tool))
    },

    "experimental.chat.system.transform": async (_input: unknown, output: { system: string[] }) => {
      output.system.push(s.problem ? problemNotice(s.problem) : SYSTEM_NOTICE)
    },
  }
}

// ---- OpenCode 2 -------------------------------------------------------------

async function setupV2(ctx: any) {
  const location = ctx.location ?? {}
  const s = await prepare([location.project?.directory, location.directory], ctx.options)
  if (s.problem) console.error(`FarHand not started: ${s.problem}`)
  if (!s.problem && !s.active) return

  // Replayed whenever the tool set changes (MCP servers connecting), so
  // tools that appear later are removed too.
  await ctx.tool.transform((editor: any) => {
    for (const tool of editor.list()) {
      if (isLocalTool(tool.id)) editor.remove(tool.id)
    }
  })

  // Code Mode exposes MCP tools through `execute`, a JavaScript sandbox
  // whose `fetch` can read local files. With it off for every server, the
  // user's other MCP tools are offered directly instead and no `execute`
  // tool is needed; the fence below refuses it if one appears anyway.
  await ctx.mcp.transform((editor: any) => {
    for (const [name] of editor.list()) {
      editor.update(name, (config: any) => {
        config.codemode = false
      })
    }
  })

  if (!s.problem) {
    await ctx.mcp.transform((editor: any) => {
      editor.set("farhand", {
        type: "local",
        command: s.command,
        // Direct tools, not Code Mode: the model then calls
        // farhand_remote_shell and friends by the names the system prompt
        // and FarHand's own instructions use, and each call meets its own
        // permission rule.
        codemode: false,
        timeout: {
          // Tool discovery waits for the SSH preflight; give slow hosts room.
          startup: 60_000,
          catalog: 60_000,
          // A remote command may run for FarHand's full maximum; the MCP
          // call must outlive it so the server can report the timeout.
          execution: (s.maxCommandSecs + 60) * 1000,
        },
      })
    })
  }

  await ctx.tool.hook("execute.before", (event: { tool: string }) => {
    if (isLocalTool(event.tool)) throw new Error(refusal(event.tool))
  })

  // "ask" in farhand.toml always prompts (a rule that denies still
  // denies); "auto" leaves OpenCode's own decision, so a stricter rule the
  // user wrote in opencode.jsonc still holds.
  await ctx.permission.hook("evaluate", (event: { action: string; effect: string }) => {
    if (!event.action.startsWith("farhand_")) return
    const mode = s.approval[event.action.slice("farhand_".length)]
    if (mode === "ask" && event.effect !== "deny") event.effect = "ask"
  })

  await ctx.session.hook("context", (event: { system: { type: "text"; text: string }[] }) => {
    event.system.push({ type: "text", text: s.problem ? problemNotice(s.problem) : SYSTEM_NOTICE })
  })
}

export default {
  id: "farhand",
  setup: setupV2,
  server: serverV1,
}
