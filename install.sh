#!/usr/bin/env bash
# Nexus CLI installer
#
# Usage:
#   curl -fsSL https://nexus.gatewarden.eu/install.sh | bash
#
# Options (via env vars):
#   NEXUS_VERSION   Pin a specific version tag, e.g. "v0.1.1" (default: latest release)
#   NEXUS_BIN_DIR   Custom install directory (default: ~/.local/bin or ~/.cargo/bin)
#   GITHUB_TOKEN    GitHub PAT for private repo access (required)
#
# The installer tries to download a pre-built binary from GitHub Releases.
# If no binary exists for the current platform it falls back to `cargo install`.

set -euo pipefail

REPO="gwnexus/nexus-cli"
BINARY_NAME="nexus"

# ── colours (disabled when piped) ──────────────────────────────────
if [ -t 1 ]; then
  BOLD="\033[1m"  GREEN="\033[32m"  YELLOW="\033[33m"
  RED="\033[31m"   CYAN="\033[36m"   RESET="\033[0m"
else
  BOLD="" GREEN="" YELLOW="" RED="" CYAN="" RESET=""
fi

info()  { printf "${BOLD}${CYAN}info${RESET}  %s\n" "$1"; }
ok()    { printf "${BOLD}${GREEN}  ok${RESET}  %s\n" "$1"; }
warn()  { printf "${BOLD}${YELLOW}warn${RESET}  %s\n" "$1"; }
err()   { printf "${BOLD}${RED} err${RESET}  %s\n" "$1" >&2; }
die()   { err "$1"; exit 1; }

# ── detect platform ────────────────────────────────────────────────
detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux*)  OS="linux"  ;;
    Darwin*) OS="darwin" ;;
    *)       die "Unsupported OS: $os" ;;
  esac

  case "$arch" in
    x86_64|amd64)  ARCH="x86_64"  ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *)             die "Unsupported architecture: $arch" ;;
  esac

  # Map to Rust target triple (must match release.yml matrix)
  case "${OS}-${ARCH}" in
    darwin-aarch64) TARGET="aarch64-apple-darwin"       ;;
    darwin-x86_64)  TARGET="x86_64-apple-darwin"        ;;
    linux-x86_64)   TARGET="x86_64-unknown-linux-gnu"   ;;
    linux-aarch64)  TARGET="aarch64-unknown-linux-gnu"  ;;
    *)              die "No target triple for ${OS}-${ARCH}" ;;
  esac

  info "Platform: ${OS}/${ARCH} -> ${TARGET}"
}

# ── resolve install dir ───────────────────────────────────────────
resolve_bin_dir() {
  if [ -n "${NEXUS_BIN_DIR:-}" ]; then
    BIN_DIR="$NEXUS_BIN_DIR"
  elif [ -d "$HOME/.local/bin" ]; then
    BIN_DIR="$HOME/.local/bin"
  elif [ -d "$HOME/.cargo/bin" ]; then
    BIN_DIR="$HOME/.cargo/bin"
  else
    BIN_DIR="$HOME/.local/bin"
  fi
  mkdir -p "$BIN_DIR"
}

# ── check GitHub auth ─────────────────────────────────────────────
check_github_auth() {
  if [ -z "${GITHUB_TOKEN:-}" ]; then
    # Try gh CLI token
    if command -v gh &>/dev/null && gh auth status &>/dev/null 2>&1; then
      GITHUB_TOKEN="$(gh auth token 2>/dev/null || true)"
    fi
  fi

  if [ -z "${GITHUB_TOKEN:-}" ]; then
    die "GITHUB_TOKEN is required (private repo). Set it via:
    export GITHUB_TOKEN=ghp_...
  or authenticate with: gh auth login"
  fi

  info "GitHub auth: ok"
}

# ── try binary download (GitHub Releases) ─────────────────────────
try_binary_download() {
  local version="${NEXUS_VERSION:-latest}"
  local api_base="https://api.github.com/repos/${REPO}"
  local api_url

  if [ "$version" = "latest" ]; then
    api_url="${api_base}/releases/latest"
  else
    api_url="${api_base}/releases/tags/${version}"
  fi

  info "Checking for pre-built binary (${version})..."

  local release_json http_code
  http_code="$(curl -sL -w "%{http_code}" -o /tmp/nexus_release.json \
    -H "Authorization: token ${GITHUB_TOKEN}" \
    -H "Accept: application/vnd.github+json" \
    "$api_url" 2>/dev/null || echo "000")"

  if [ "$http_code" != "200" ]; then
    warn "No release found (HTTP ${http_code}) -- falling back to source build"
    return 1
  fi

  release_json="$(cat /tmp/nexus_release.json)"
  rm -f /tmp/nexus_release.json

  # Expected asset name: nexus-<target>.tar.gz  (matches release.yml)
  local asset_name="nexus-${TARGET}.tar.gz"
  local asset_id
  asset_id="$(echo "$release_json" \
    | python3 -c "
import sys, json
data = json.load(sys.stdin)
for a in data.get('assets', []):
    if a['name'] == '${asset_name}':
        print(a['id'])
        break
" 2>/dev/null || echo "")"

  if [ -z "$asset_id" ]; then
    warn "No asset '${asset_name}' in release -- falling back to source build"
    return 1
  fi

  info "Downloading ${asset_name} (asset ${asset_id})..."

  local tmp_dir
  tmp_dir="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp_dir'" EXIT

  # Private repo: download via API (browser_download_url requires browser auth)
  curl -fsSL \
    -H "Authorization: token ${GITHUB_TOKEN}" \
    -H "Accept: application/octet-stream" \
    -o "${tmp_dir}/${asset_name}" \
    "${api_base}/releases/assets/${asset_id}"

  # Verify checksum if available
  local sha_id
  sha_id="$(echo "$release_json" \
    | python3 -c "
import sys, json
data = json.load(sys.stdin)
for a in data.get('assets', []):
    if a['name'] == '${asset_name}.sha256':
        print(a['id'])
        break
" 2>/dev/null || echo "")"

  if [ -n "$sha_id" ]; then
    curl -fsSL \
      -H "Authorization: token ${GITHUB_TOKEN}" \
      -H "Accept: application/octet-stream" \
      -o "${tmp_dir}/${asset_name}.sha256" \
      "${api_base}/releases/assets/${sha_id}"

    info "Verifying SHA256 checksum..."
    cd "$tmp_dir"
    if command -v sha256sum &>/dev/null; then
      sha256sum -c "${asset_name}.sha256"
    elif command -v shasum &>/dev/null; then
      shasum -a 256 -c "${asset_name}.sha256"
    fi
    cd - >/dev/null
  fi

  # Extract: tar.gz contains nexus-<target>/nexus
  tar -xzf "${tmp_dir}/${asset_name}" -C "$tmp_dir"

  local extracted_binary="${tmp_dir}/nexus-${TARGET}/${BINARY_NAME}"
  if [ ! -f "$extracted_binary" ]; then
    # Fallback: binary might be at root level
    extracted_binary="${tmp_dir}/${BINARY_NAME}"
  fi

  if [ ! -f "$extracted_binary" ]; then
    warn "Binary not found in archive -- falling back to source build"
    return 1
  fi

  mv "$extracted_binary" "${BIN_DIR}/${BINARY_NAME}"
  chmod +x "${BIN_DIR}/${BINARY_NAME}"

  local release_tag
  release_tag="$(echo "$release_json" | python3 -c "import sys,json; print(json.load(sys.stdin).get('tag_name','?'))" 2>/dev/null || echo "?")"

  ok "Installed ${BINARY_NAME} ${release_tag} -> ${BIN_DIR}/${BINARY_NAME}"
  return 0
}

# ── build from source (fallback) ──────────────────────────────────
build_from_source() {
  if ! command -v cargo &>/dev/null; then
    die "No pre-built binary for ${TARGET} and Rust toolchain not found.
  Install Rust first: https://rustup.rs
  Then re-run this installer."
  fi

  local rust_ver major minor
  rust_ver="$(rustc --version | awk '{print $2}')"
  major="$(echo "$rust_ver" | cut -d. -f1)"
  minor="$(echo "$rust_ver" | cut -d. -f2)"
  if [ "$major" -lt 1 ] || { [ "$major" -eq 1 ] && [ "$minor" -lt 85 ]; }; then
    die "Rust >= 1.85 required (found ${rust_ver}). Run: rustup update stable"
  fi

  info "Building from source with cargo (Rust ${rust_ver})..."

  local repo_url="https://${GITHUB_TOKEN}@github.com/${REPO}.git"
  local version="${NEXUS_VERSION:-}"
  local version_args=()
  if [ -n "$version" ] && [ "$version" != "latest" ]; then
    version_args=(--tag "$version")
  fi

  # RUSTFLAGS="" avoids nix/devbox linker issues on macOS
  RUSTFLAGS="" cargo install \
    --git "$repo_url" \
    "${version_args[@]}" \
    nexusctl \
    --locked 2>&1 || {
      warn "Retrying without --locked..."
      RUSTFLAGS="" cargo install \
        --git "$repo_url" \
        "${version_args[@]}" \
        nexusctl 2>&1
    }

  ok "Built and installed from source"
}

# ── verify ────────────────────────────────────────────────────────
verify_install() {
  if command -v "$BINARY_NAME" &>/dev/null; then
    local ver
    ver="$("$BINARY_NAME" --version 2>/dev/null || echo "unknown")"
    ok "${ver}"
  elif [ -x "${BIN_DIR}/${BINARY_NAME}" ]; then
    local ver
    ver="$("${BIN_DIR}/${BINARY_NAME}" --version 2>/dev/null || echo "unknown")"
    ok "${ver}"
    warn "${BIN_DIR} is not in your PATH. Add it:"
    echo ""
    echo "  export PATH=\"${BIN_DIR}:\$PATH\""
    echo ""
  else
    die "Installation failed -- '${BINARY_NAME}' binary not found"
  fi
}

# ── main ──────────────────────────────────────────────────────────
main() {
  echo ""
  printf "${BOLD}Nexus CLI Installer${RESET}\n"
  echo "============================="
  echo ""

  detect_platform
  resolve_bin_dir
  check_github_auth

  if ! try_binary_download; then
    build_from_source
  fi

  verify_install

  echo ""
  ok "Done! Run '${BINARY_NAME} --help' to get started."
  echo ""
  echo "  Quick start:"
  echo "    ${BINARY_NAME} login          # authenticate with Nexus platform"
  echo "    ${BINARY_NAME} init           # scaffold your project"
  echo "    ${BINARY_NAME} preflight      # verify environment readiness"
  echo ""
}

main "$@"
