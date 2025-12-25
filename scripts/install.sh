#!/usr/bin/env sh
set -eu

REPO="${PX_REPO:-ck-zhang/px-dev}"
VERSION="${PX_VERSION:-}"
INSTALL_DIR="${PX_INSTALL_DIR:-$HOME/.local/bin}"

usage() {
  cat <<'EOF'
px installer

Usage:
  install.sh [--version <tag>] [--dir <path>] [--repo <owner/repo>]

Environment overrides:
  PX_VERSION, PX_INSTALL_DIR, PX_REPO
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      VERSION="${2:-}"; shift 2;;
    --dir)
      INSTALL_DIR="${2:-}"; shift 2;;
    --repo)
      REPO="${2:-}"; shift 2;;
    -h|--help)
      usage; exit 0;;
    *)
      echo "error: unknown argument: $1" >&2
      usage
      exit 2;;
  esac
done

if [ -z "${REPO}" ]; then
  echo "error: PX_REPO is empty" >&2
  exit 2
fi

os="$(uname -s)"
case "${os}" in
  Linux) os="Linux" ;;
  Darwin) os="macOS" ;;
  *)
    echo "error: unsupported OS: ${os}" >&2
    exit 2
    ;;
esac

arch="$(uname -m)"

if [ -z "${VERSION}" ]; then
  api="https://api.github.com/repos/${REPO}/releases/latest"
  json="$(curl -fsSL "${api}")"
  VERSION="$(printf '%s' "${json}" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
fi

if [ -z "${VERSION}" ]; then
  echo "error: could not determine latest release tag (set PX_VERSION or --version)" >&2
  exit 2
fi

asset="px-${VERSION}-${os}-${arch}.tar.gz"
sha_asset="${asset}.sha256"
base="https://github.com/${REPO}/releases/download/${VERSION}"

tmp="$(mktemp -d)"
cleanup() { rm -rf "${tmp}"; }
trap cleanup EXIT

mkdir -p "${INSTALL_DIR}"

echo "px: downloading ${asset} (${REPO}@${VERSION})"
curl -fsSL "${base}/${asset}" -o "${tmp}/${asset}"

if curl -fsSL "${base}/${sha_asset}" -o "${tmp}/${sha_asset}"; then
  expected="$(awk '{print $1}' "${tmp}/${sha_asset}" | head -n1)"
  actual=""
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "${tmp}/${asset}" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "${tmp}/${asset}" | awk '{print $1}')"
  fi
  if [ -n "${actual}" ] && [ "${expected}" != "${actual}" ]; then
    echo "error: sha256 mismatch for ${asset}" >&2
    echo "expected: ${expected}" >&2
    echo "actual:   ${actual}" >&2
    exit 1
  fi
fi

tar -xzf "${tmp}/${asset}" -C "${tmp}"
if [ ! -f "${tmp}/px" ]; then
  echo "error: expected px binary in archive" >&2
  exit 1
fi

install -m 0755 "${tmp}/px" "${INSTALL_DIR}/px"
echo "px: installed to ${INSTALL_DIR}/px"

if ! command -v px >/dev/null 2>&1; then
  echo "px: note: add ${INSTALL_DIR} to your PATH"
fi
