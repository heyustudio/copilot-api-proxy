#!/usr/bin/env bash
#
# copilot-api-proxy installer for macOS and Linux.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/heyustudio/copilot-api-proxy/main/install.sh | bash
#
# Environment variables:
#   INSTALL_DIR  Install location (default: ~/.local/bin)
#   VERSION      Release tag to install (default: latest)
#   SKIP_AUTH    Set to any non-empty value to skip the GitHub device-flow auth step
#   SKIP_SETUP   Set to any non-empty value to skip generating the claude-proxy launcher
#
set -euo pipefail

REPO="heyustudio/copilot-api-proxy"
BINARY="copilot-api-proxy"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${VERSION:-}"

tmpdir=""
cleanup() { [ -n "$tmpdir" ] && rm -rf "$tmpdir"; }
trap cleanup EXIT

# Set by ensure_on_path when it appends to a shell rc file.
PATH_RC_UPDATED=""

err()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

detect_target() {
  local os arch
  case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux)  os="unknown-linux-gnu" ;;
    *) err "unsupported OS: $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64|amd64)  arch="x86_64" ;;
    *) err "unsupported architecture: $(uname -m)" ;;
  esac
  if [ "$os" = "apple-darwin" ] && [ "$arch" = "x86_64" ]; then
    err "macOS Intel (x86_64) is not currently published as a prebuilt binary. Build from source instead: https://github.com/$REPO#from-source"
  fi
  printf '%s-%s' "$arch" "$os"
}

resolve_tag() {
  if [ -n "$VERSION" ]; then
    printf '%s' "$VERSION"
    return
  fi
  local tag
  tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep -m1 '"tag_name"' \
    | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
  [ -n "$tag" ] || err "could not resolve latest release tag from GitHub API"
  printf '%s' "$tag"
}

verify_checksum() {
  local dir="$1" file="$2"
  if command -v shasum >/dev/null 2>&1; then
    ( cd "$dir" && shasum -a 256 -c "$file.sha256" >/dev/null )
  elif command -v sha256sum >/dev/null 2>&1; then
    ( cd "$dir" && sha256sum -c "$file.sha256" >/dev/null )
  else
    warn "no sha256 tool available — skipping checksum verification"
    return 0
  fi
}

# Ensure $1 is on PATH for future shells by appending an export to the right rc
# file. Idempotent: skips when already active or already configured. Also exports
# PATH for the rest of this install so generated hints resolve.
ensure_on_path() {
  local dir="$1"

  case ":$PATH:" in
    *":$dir:"*) return 0 ;;  # already active this session
  esac

  local rc
  case "${SHELL:-}" in
    */zsh) rc="$HOME/.zshrc" ;;
    */bash)
      if [ "$(uname -s)" = "Darwin" ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi
      ;;
    *) rc="$HOME/.profile" ;;
  esac

  # Write a $HOME-relative path when possible so the rc stays portable.
  local rc_dir="$dir"
  case "$dir" in
    "$HOME"/*) rc_dir="\$HOME${dir#"$HOME"}" ;;
  esac
  local line="export PATH=\"$rc_dir:\$PATH\""

  if [ -f "$rc" ] && grep -Fq "$line" "$rc"; then
    warn "$dir is configured in $rc but not active in this shell."
    PATH_RC_UPDATED="$rc"
    export PATH="$dir:$PATH"
    return 0
  fi

  if ! { printf '\n# Added by copilot-api-proxy installer\n%s\n' "$line" >> "$rc"; }; then
    warn "could not update $rc — add this line manually: $line"
    return 0
  fi

  export PATH="$dir:$PATH"
  PATH_RC_UPDATED="$rc"
  info "Added $dir to your PATH in $rc"
}

print_path_reminder() {
  [ -n "$PATH_RC_UPDATED" ] || return 0
  echo
  warn "Restart your shell or run: source $PATH_RC_UPDATED"
  echo "         …so 'copilot-api-proxy' and 'claude-proxy' resolve as commands."
}

main() {
  command -v curl >/dev/null 2>&1 || err "curl is required"
  command -v tar  >/dev/null 2>&1 || err "tar is required"

  local target tag asset url extracted_dir dest
  target="$(detect_target)"
  info "Platform: $target"

  tag="$(resolve_tag)"
  info "Version:  $tag"

  asset="${BINARY}-${tag}-${target}.tar.gz"
  url="https://github.com/$REPO/releases/download/$tag/$asset"

  tmpdir="$(mktemp -d)"

  info "Downloading $asset"
  curl -fsSL "$url"        -o "$tmpdir/$asset"        || err "failed to download $url"
  curl -fsSL "$url.sha256" -o "$tmpdir/$asset.sha256" || err "failed to download $url.sha256"

  info "Verifying checksum"
  verify_checksum "$tmpdir" "$asset"

  info "Extracting"
  tar -xzf "$tmpdir/$asset" -C "$tmpdir"
  extracted_dir="$tmpdir/${BINARY}-${tag}-${target}"
  [ -f "$extracted_dir/$BINARY" ] || err "binary missing from archive: $extracted_dir/$BINARY"

  mkdir -p "$INSTALL_DIR"
  dest="$INSTALL_DIR/$BINARY"
  install -m 0755 "$extracted_dir/$BINARY" "$dest"
  info "Installed: $dest"

  if [ "$(uname -s)" = "Darwin" ]; then
    xattr -d com.apple.quarantine "$dest" 2>/dev/null || true
  fi

  ensure_on_path "$INSTALL_DIR"

  if [ -n "${SKIP_AUTH:-}" ]; then
    info "SKIP_AUTH set — skipping device-flow auth"
    info "Done. Next: $dest auth && $dest claude-setup && $dest server"
    print_path_reminder
    return 0
  fi

  info "Starting GitHub device-flow authentication..."
  echo
  # Not exec'd: we keep the controlling terminal so the claude-setup picker
  # (which reads /dev/tty) can run afterwards.
  "$dest" auth || err "authentication failed"

  if [ -n "${SKIP_SETUP:-}" ]; then
    info "SKIP_SETUP set — skipping launcher generation"
    info "Done. Generate it later with: $dest claude-setup"
    print_path_reminder
    return 0
  fi

  echo
  info "Generating Claude Code launcher (claude-proxy)..."
  "$dest" claude-setup --output "$INSTALL_DIR/claude-proxy" || \
    warn "launcher generation failed — run '$dest claude-setup' manually later"

  print_path_reminder
}

main "$@"
