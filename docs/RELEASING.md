# Releasing

Three registries, one version number. `Cargo.toml` (`workspace.package.version`)
and `packages/npm/farhand/package.json` must agree; the npm wrapper fetches the
release whose tag equals its own version.

1. Bump the version in both files, run the four gates, commit.
2. Tag and push: `git tag vX.Y.Z && git push origin main vX.Y.Z`.
   The Release workflow builds the four targets and publishes the GitHub
   release with `SHA256SUMS`. Wait for it to finish.
3. crates.io, in dependency order (needs `cargo login`):
   `cargo publish -p farhand-core` then `cargo publish -p farhand`.
4. npm (needs `npm login`): `cd packages/npm/farhand && npm publish`.
   Test with `npx farhand@X.Y.Z --help` from an empty directory.
