#!/bin/sh
# Install a prebuilt farhand binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/CogFlux/farhand/main/install.sh | sh
#
# Environment:
#   FARHAND_VERSION   tag to install (default: latest release)
#   FARHAND_BIN_DIR   where to put the binary (default: ~/.local/bin)
#
# Downloads farhand-<version>-<target>.tar.gz, verifies it against SHA256SUMS,
# and copies one file. Nothing else is written.
set -eu

repo="CogFlux/farhand"
bin_dir="${FARHAND_BIN_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin) os_t="apple-darwin" ;;
  Linux)  os_t="unknown-linux-gnu" ;;
  *) echo "farhand: unsupported OS: $os (macOS and Linux only; Windows is not supported)" >&2; exit 1 ;;
esac
case "$arch" in
  arm64|aarch64) arch_t="aarch64" ;;
  x86_64|amd64)  arch_t="x86_64" ;;
  *) echo "farhand: unsupported architecture: $arch" >&2; exit 1 ;;
esac
target="${arch_t}-${os_t}"

if [ -n "${FARHAND_VERSION:-}" ]; then
  tag="$FARHAND_VERSION"
else
  tag="$(curl -fsSL "https://api.github.com/repos/${repo}/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
  [ -n "$tag" ] || { echo "farhand: cannot determine the latest release" >&2; exit 1; }
fi
version="${tag#v}"
name="farhand-${version}-${target}"
base="https://github.com/${repo}/releases/download/${tag}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "farhand: downloading ${name}.tar.gz"
curl -fsSL -o "${tmp}/${name}.tar.gz" "${base}/${name}.tar.gz"
curl -fsSL -o "${tmp}/SHA256SUMS" "${base}/SHA256SUMS"

expected="$(grep " ${name}.tar.gz\$" "${tmp}/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$expected" ] || { echo "farhand: no checksum for ${name}.tar.gz" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tmp}/${name}.tar.gz" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "${tmp}/${name}.tar.gz" | cut -d' ' -f1)"
fi
[ "$expected" = "$actual" ] || { echo "farhand: checksum mismatch" >&2; exit 1; }

tar -C "$tmp" -xzf "${tmp}/${name}.tar.gz"
mkdir -p "$bin_dir"
install -m 755 "${tmp}/${name}/farhand" "${bin_dir}/farhand"
echo "farhand: installed ${tag} to ${bin_dir}/farhand"
case ":$PATH:" in
  *":${bin_dir}:"*) ;;
  *) echo "farhand: add ${bin_dir} to your PATH, or run it by full path" ;;
esac
echo "farhand: next, \`farhand install opencode\` (or claude-code / codex), then \`farhand init > .farhand.toml\` in a project"
