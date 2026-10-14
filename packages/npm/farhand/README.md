# farhand (npm wrapper)

`npx farhand` / `npm i -g farhand` runs the prebuilt `farhand` binary for
your platform. On first run it downloads
`farhand-<version>-<target>.tar.gz` from the matching GitHub release,
verifies it against `SHA256SUMS`, and keeps it inside this package. The
version of this package always equals the binary version it fetches.

Install it globally to set up an agent:

```
npm i -g farhand
farhand install claude-code    # or opencode, codex
```

The agent's config then launches this package's entry (`bin/farhand.js`),
not the versioned binary, so `npm i -g farhand@latest` upgrades the agent
too; the first run after an upgrade downloads the new binary. The agent
needs `node` on its `PATH`. `npx farhand init` / `npx farhand check` work
for a try, but `farhand install` refuses to run from npx's cache (or
`pnpm dlx`, `bunx`): the agent would lose FarHand when the cache is
cleared.

Everything else — what FarHand is, how to configure it, how it plugs into
OpenCode, Claude Code and Codex — is in the
[main README](https://github.com/CogFlux/farhand).
