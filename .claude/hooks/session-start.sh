#!/bin/bash
# SessionStart hook for Claude Code on the web.
#
# Installs mise (https://mise.jdx.dev) if needed and uses it to install the
# toolchains declared in mise.toml, so that `cargo`, `clippy`, `rustfmt` and the
# `mise run <task>` commands are available during the session.
#
# Runs synchronously: the session waits until dependencies are installed, which
# avoids races where tests/linters run before the toolchain exists. Switch to
# async mode (see the skill docs) if faster startup is preferred.
set -euo pipefail

# Only do anything in the remote (Claude Code on the web) environment. Locally,
# developers manage their own mise installation.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$(pwd)}"
export MISE_DATA_DIR="${MISE_DATA_DIR:-$HOME/.local/share/mise}"
MISE_SHIMS="$MISE_DATA_DIR/shims"
LOCAL_BIN="$HOME/.local/bin"
mkdir -p "$LOCAL_BIN"

# Map `uname` output to mise's release asset naming (os-arch).
mise_asset_slug() {
  local os arch
  case "$(uname -s)" in
    Linux) os="linux" ;;
    Darwin) os="macos" ;;
    *) os="linux" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch="x64" ;;
    arm64 | aarch64) arch="arm64" ;;
    armv7l) arch="armv7" ;;
    *) arch="x64" ;;
  esac
  echo "${os}-${arch}"
}

# Fallback installer: download the standalone mise binary straight from GitHub
# releases. Used when https://mise.run is unreachable (e.g. a restrictive
# network policy that still allows github.com).
install_mise_from_github() {
  local slug version url
  slug="$(mise_asset_slug)"
  version="$(curl -fsSL -o /dev/null -w '%{url_effective}' \
    https://github.com/jdx/mise/releases/latest | grep -oE 'v[0-9.]+$' || true)"
  if [ -z "$version" ]; then
    echo "could not resolve latest mise version from GitHub" >&2
    return 1
  fi
  url="https://github.com/jdx/mise/releases/download/${version}/mise-${version}-${slug}"
  echo "downloading mise ${version} (${slug}) from GitHub releases..."
  curl -fsSL "$url" -o "$LOCAL_BIN/mise"
  chmod +x "$LOCAL_BIN/mise"
}

# Locate mise, installing it if it isn't already on PATH.
if command -v mise >/dev/null 2>&1; then
  MISE_BIN="$(command -v mise)"
else
  echo "mise not found; installing..."
  if curl -fsSL https://mise.run | sh; then
    MISE_BIN="$LOCAL_BIN/mise"
  else
    echo "https://mise.run unreachable; falling back to GitHub releases" >&2
    install_mise_from_github
    MISE_BIN="$LOCAL_BIN/mise"
  fi
fi

# Make this shell able to call mise and its installed tools.
export PATH="$LOCAL_BIN:$MISE_SHIMS:$PATH"

# Let mise authenticate to GitHub when a token is available; this avoids the low
# unauthenticated rate limit while resolving toolchain versions. Only export a
# non-empty token, so mise still falls back to unauthenticated requests.
if [ -z "${GITHUB_TOKEN:-}" ] && [ -n "${GH_TOKEN:-}" ]; then
  export GITHUB_TOKEN="$GH_TOKEN"
fi

cd "$PROJECT_DIR"

# Trust the repo config and install the declared toolchains (idempotent). A
# failure here (e.g. a network policy that blocks toolchain downloads) should
# not abort the whole session — any preinstalled toolchain remains usable — so
# we report it and carry on.
"$MISE_BIN" trust --yes >/dev/null 2>&1 || true
if "$MISE_BIN" install; then
  echo "mise dependencies installed:"
  "$MISE_BIN" ls --installed || true
else
  echo "warning: 'mise install' did not complete; the network policy may block" >&2
  echo "toolchain downloads. Widen the policy or set GITHUB_TOKEN if needed." >&2
fi

# Persist mise + the installed tools onto PATH for the rest of the session.
# Keep any preexisting cargo install on PATH too, as a fallback toolchain.
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  {
    echo "export PATH=\"$LOCAL_BIN:$MISE_SHIMS:\$HOME/.cargo/bin:\$PATH\""
    echo "export MISE_DATA_DIR=\"$MISE_DATA_DIR\""
  } >> "$CLAUDE_ENV_FILE"
fi
