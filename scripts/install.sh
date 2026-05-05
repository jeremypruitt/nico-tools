#!/usr/bin/env bash
# Install nico-doctor and nico-correlate from the latest GitHub release.
# Detects OS and architecture automatically.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/jeremypruitt/nico-tools/main/scripts/install.sh | bash
#
# Override install directory:
#   INSTALL_DIR=/usr/bin curl -fsSL ... | bash

set -euo pipefail

REPO="jeremypruitt/nico-tools"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

# Detect OS
case "$(uname -s)" in
  Darwin) OS="apple-darwin" ;;
  Linux)  OS="unknown-linux-gnu" ;;
  *) echo "error: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

# Detect architecture
case "$(uname -m)" in
  x86_64|amd64)    ARCH="x86_64" ;;
  arm64|aarch64)   ARCH="aarch64" ;;
  *) echo "error: unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

TARGET="${ARCH}-${OS}"

# Resolve latest release tag
VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep '"tag_name"' \
  | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "${VERSION}" ]; then
  echo "error: could not determine latest release version" >&2
  exit 1
fi

ARCHIVE="nico-tools-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"

echo "Installing nico-tools ${VERSION} (${TARGET}) → ${INSTALL_DIR}"

TMP=$(mktemp -d)
trap 'rm -rf "${TMP}"' EXIT

curl -fsSL "${URL}" -o "${TMP}/${ARCHIVE}"
tar -xzf "${TMP}/${ARCHIVE}" -C "${TMP}"

for BIN in nico-doctor nico-correlate; do
  if [ -f "${TMP}/${BIN}" ]; then
    install -m 755 "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
    echo "  installed ${BIN}"
  else
    echo "warning: ${BIN} not found in archive" >&2
  fi
done

echo "Done. Run 'nico-doctor --help' to verify."
