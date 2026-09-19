/**
 * FarHand entry point for OpenCode.
 *
 * This file is deliberately thin. All the real work — the SSH session, the
 * secret guard, the local allowlist, the audit log — lives in the `farhand`
 * binary, which speaks MCP. The plugin does three things:
 *
 *   1. registers `farhand` as a local MCP server and turns every local
 *      shell / file tool off, so the model's only hands are the remote ones;
 *   2. refuses any local tool call that slips through, as a second fence;
 *   3. tells the model, in the system prompt, where it is working.
 *
 * Install: copy or symlink this file into `~/.config/opencode/plugins/` or a
 * project's `.opencode/plugins/`. The `farhand` binary must be on PATH, or
 * set FARHAND_BIN. Configuration is read by the binary itself (see
 * `farhand init`); a `.farhand.toml` at the project root takes precedence.
 */

import type { Plugin } from "@opencode-ai/plugin"
import { existsSync } from "node:fs"
import { join } from "node:path"

/** OpenCode's built-in tools that touch the local machine. */
const LOCAL_TOOLS = [
  "bash",
  "read",
  "write",
  "edit",
  "multiedit",
  "patch",
  "glob",
  "grep",
  "list",
  "ls",
  "lsp",
  "lsp_diagnostics",
  "lsp_hover",
]

/**
 * Approval comes from FarHand's own config (`[approval]` in farhand.toml):
 * `farhand validate --json` reports, per mutating tool, "ask" or "auto", and
 * this plugin translates that into OpenCode's permission system. Read-only
 * tools never prompt. Any `farhand*` key the user has put in their own
 * `permission` config still wins over the translated values.
 */
type Approval = Record<string, "ask" | "auto">

/** Replaced with the binary's absolute path by `farhand install opencode`. */
const BUILT_IN_BIN = "farhand"
/** Set by `farhand install opencode --config PATH`: one fixed config for every session. */
const BUILT_IN_CONFIG: string | undefined = undefined

const SYSTEM_NOTICE = [
  "FarHand is active: this session operates on a REMOTE host over SSH.",
  "\"The current directory\", \"here\", \"this project\" and every relative path mean the remote workdir, not this machine.",
  "The user sits at the local machine: a service started on the remote is NOT at localhost for them —",
  "report the remote's address (see farhand_remote_info) or an `ssh -L` tunnel command.",
  "The local bash/read/write/edit/glob/grep/list tools are disabled and will fail if called.",
  "Use the farhand_remote_* tools for every command and file operation (farhand_remote_ls to list files).",
  "Use farhand_local_ls / farhand_local_read only when the user explicitly asks about their local folders,",
  "and farhand_upload / farhand_download to move files between them and the remote.",
  "Call farhand_remote_info once if you need to know the host, workdir or allowed local folders.",
  "Credential-like content is refused by the server; never try to work around a refusal.",
].join(" ")

export const FarHandPlugin: Plugin = async ({ worktree, directory, client, $ }, options) => {
  const bin = (options?.bin as string | undefined) ?? process.env.FARHAND_BIN ?? BUILT_IN_BIN
  const projectConfig = [worktree, directory]
    .filter(Boolean)
    .map((d) => join(d, ".farhand.toml"))
    .find((p) => existsSync(p))
  const explicit = (options?.config as string | undefined) ?? BUILT_IN_CONFIG
  const configPath = explicit ?? projectConfig

  const configArgs = configPath ? ["--config", configPath] : []
  const command = [bin, "serve", ...configArgs, "--cwd", worktree || directory]

  // Parse the config up front (no network) so a bad file shows up as a
  // readable message instead of OpenCode's bare "failed". The local tools
  // stay off either way: a misconfigured remote must not silently turn
  // into a local session.
  let problem: string | undefined
  let active = true
  let approval: Approval = {}
  const cwdArgs = ["--cwd", worktree || directory]
  try {
    const result = await $`${bin} validate --json ${configArgs} ${cwdArgs}`.quiet().nothrow()
    const stdout = result.stdout.toString().trim()
    let parsed: { ok?: boolean; code?: string; error?: string; active?: boolean; approval?: Approval } = {}
    try {
      parsed = JSON.parse(stdout)
    } catch {
      /* not JSON: an old binary or a crash; fall through to the exit code */
    }
    if (parsed.code === "no-config") {
      // Nothing configured anywhere: not a FarHand session.
      active = false
    } else if (result.exitCode !== 0 || !parsed.ok) {
      problem = parsed.error ?? (result.stderr.toString() || stdout).trim() ?? `exit ${result.exitCode}`
    } else {
      approval = parsed.approval ?? {}
      active = parsed.active !== false
    }
  } catch (e) {
    problem = `cannot run \`${bin}\`: ${e instanceof Error ? e.message : String(e)}`
  }
  if (problem) {
    client.tui
      .showToast({
        body: { title: "FarHand not started", message: problem, variant: "error", duration: 15_000 },
      })
      .catch(() => {})
  }

  // Not active here (global `activation = "project"`, no .farhand.toml):
  // an ordinary local session, and this plugin does nothing at all.
  if (!problem && !active) return {}

  return {
    config: async (config) => {
      if (!problem) {
        config.mcp = {
          ...config.mcp,
          farhand: {
            type: "local",
            command,
            enabled: true,
            // Tool discovery waits for the SSH preflight; give slow hosts room.
            timeout: 60_000,
          },
        }
      }
      const disabled = Object.fromEntries(LOCAL_TOOLS.map((t) => [t, false]))
      config.tools = { ...config.tools, ...disabled }

      const permission = (config.permission ?? {}) as Record<string, unknown>
      for (const [tool, mode] of Object.entries(approval)) {
        const key = `farhand_${tool}`
        if (key in permission) continue // the user's own rule wins
        permission[key] = mode === "auto" ? "allow" : "ask"
      }
      config.permission = permission as typeof config.permission
    },

    "tool.execute.before": async (input) => {
      if (LOCAL_TOOLS.includes(input.tool)) {
        throw new Error(
          `FarHand: local tool "${input.tool}" is disabled in this session. ` +
            `Use farhand_remote_${input.tool === "list" ? "ls" : input.tool} instead.`,
        )
      }
    },

    "experimental.chat.system.transform": async (_input, output) => {
      output.system.push(
        problem
          ? `FarHand is configured for this session but could not start: ${problem}. ` +
              "Local shell and file tools are disabled and no remote tools are available. " +
              "Tell the user to fix the FarHand configuration (run `farhand check`) and do not attempt any file or shell work."
          : SYSTEM_NOTICE,
      )
    },
  }
}
