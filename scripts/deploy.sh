#!/usr/bin/env bash
set -euo pipefail

# Deploy script for building and packaging the oj command-line program.
# Builds the oj binary and all first-party plugins (release) into bin/,
# then packages them into a versioned tarball.
#
#   bin/oj                  -> main executable
#   bin/plugins/<triple>/   -> plugin cdylib artifacts
#
# The build+placement is delegated to cargo xtask (tools/xtask):
#   cargo xtask build       # oj + all plugins -> bin/
#
# Output: dist/oj-v<version>.tar.gz (containing oj and plugins/<triple>/).

# Configuration
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${PROJECT_ROOT}/dist"
BINARY_NAME="oj"

# Get version from Cargo.toml
VERSION=$(grep -m1 '^version =' "${PROJECT_ROOT}/oj/Cargo.toml" | cut -d'"' -f2)
if [[ -z "$VERSION" ]]; then
  echo "Error: Could not determine version from oj/Cargo.toml"
  exit 1
fi

PACKAGE_NAME="oj-v${VERSION}"
TEMP_DIR="${DIST_DIR}/${PACKAGE_NAME}"

# Clean dist directory
rm -rf "${DIST_DIR}"
mkdir -p "${DIST_DIR}"

# Build main binary + plugins into bin/ (release only).
echo "Building release (oj + plugins) into bin/ ..."
cargo xtask build

# Validate artifacts
BIN="${PROJECT_ROOT}/bin/${BINARY_NAME}"
if [[ ! -x "$BIN" ]]; then
  echo "Error: main binary not found at ${BIN}"
  exit 1
fi

TRIPLE_DIR=$(ls -d "${PROJECT_ROOT}/bin/plugins/"* 2>/dev/null | head -1)
if [[ -z "$TRIPLE_DIR" ]]; then
  echo "Error: no plugin artifacts under ${PROJECT_ROOT}/bin/plugins/"
  exit 1
fi

# Assemble package
mkdir -p "${TEMP_DIR}"
cp "${BIN}" "${TEMP_DIR}/${BINARY_NAME}"
cp -R "${TRIPLE_DIR}" "${TEMP_DIR}/plugins/"
chmod +x "${TEMP_DIR}/${BINARY_NAME}"

# Create tarball
TARBALL="${DIST_DIR}/${PACKAGE_NAME}.tar.gz"
tar -czf "${TARBALL}" -C "${DIST_DIR}" "${PACKAGE_NAME}"
rm -rf "${TEMP_DIR}"

echo "Deployment complete!"
echo "Package: ${TARBALL}"
ls -la "${TARBALL}"
