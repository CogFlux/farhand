# farhand (npm wrapper)

`npx farhand` / `npm i -g farhand` runs the prebuilt `farhand` binary for
your platform. On first run it downloads
`farhand-<version>-<target>.tar.gz` from the matching GitHub release,
verifies it against `SHA256SUMS`, and keeps it inside this package. The
version of this package always equals the binary version it fetches.

Everything else — what FarHand is, how to configure it, how it plugs into
OpenCode, Claude Code and Codex — is in the
[main README](https://github.com/CogFlux/farhand).
