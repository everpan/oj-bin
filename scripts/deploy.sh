#!/usr/bin/env bash
set -euo pipefail

# Deploy script for building and packaging the oj command-line program.
# Creates versioned tarballs for each target platform.
# Supports building for host platform or specific target triples.
# Output: dist/oj-v<version>-<target>.tar.gz (or without target for host)

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

# Parse command line arguments
TARGETS=()  # Array to hold target triples
CLEAN=0
VERBOSE=0

while [[ $# -gt 0 ]]; do
  case $1 in
    --help|-h)
      echo "Usage: $0 [--targets TARGET1,TARGET2,...] [--clean] [--verbose]"
      echo "  --targets TARGET1,TARGET2,...  Target triples for cross-compilation (e.g., x86_64-apple-darwin,aarch64-unknown-linux-gnu)"
      echo "                                 If omitted, builds for the host platform."
      echo "  --clean                        Clean the dist directory before building"
      echo "  --verbose                      Enable verbose output"
      echo "  --help, -h                     Show this help message"
      exit 0
      ;;
    --targets)
      IFS=',' read -ra TARGET_ARRAY <<< "$2"
      TARGETS=("${TARGET_ARRAY[@]}")
      shift 2
      ;;
    --clean)
      CLEAN=1
      shift
      ;;
    --verbose)
      VERBOSE=1
      shift
      ;;
    *)
      echo "Unknown option: $1"
      exit 1
      ;;
  esac
done

# Function to build and package for a specific target
package_target() {
  local target="$1"
  local package_name=""
  local temp_dir=""
  local binary_path=""

  if [[ -n "$target" ]]; then
    package_name="oj-v${VERSION}-${target}"
    temp_dir="${DIST_DIR}/${package_name}"
    binary_path="${PROJECT_ROOT}/target/${target}/release/${BINARY_NAME}"
  else
    package_name="oj-v${VERSION}"
    temp_dir="${DIST_DIR}/${package_name}"
    binary_path="${PROJECT_ROOT}/target/release/${BINARY_NAME}"
  fi

  if [[ $VERBOSE -eq 1 ]]; then
    echo "Processing target: ${target:-host}"
    echo "Package name: ${package_name}"
    echo "Binary path: ${binary_path}"
  fi

  # Create temporary directory
  mkdir -p "${temp_dir}"

  # Copy binary to temporary directory
  if [[ ! -f "$binary_path" ]]; then
    echo "Error: Binary not found at ${binary_path}"
    exit 1
  fi
  cp "${binary_path}" "${temp_dir}/${BINARY_NAME}"

  # Make binary executable (ensure it is)
  chmod +x "${temp_dir}/${BINARY_NAME}"

  # Create tarball
  local tarball="${DIST_DIR}/${package_name}.tar.gz"
  if [[ $VERBOSE -eq 1 ]]; then
    echo "Creating tarball: ${tarball}"
  fi
  tar -czf "${tarball}" -C "${DIST_DIR}" "$(basename "${package_name}")"

  # Clean up temporary directory
  rm -rf "${temp_dir}"

  if [[ $VERBOSE -eq 1 ]]; then
    echo "Created: ${tarball}"
  fi
}

# Clean dist directory if requested
if [[ $CLEAN -eq 1 ]]; then
  if [[ $VERBOSE -eq 1 ]]; then
    echo "Cleaning dist directory: ${DIST_DIR}"
  fi
  rm -rf "${DIST_DIR}"
fi

# Create dist directory
mkdir -p "${DIST_DIR}"

# Build for each target or host if none specified
if [[ ${#TARGETS[@]} -eq 0 ]]; then
  # Build for host
  if [[ $VERBOSE -eq 1 ]]; then
    echo "Building for host platform..."
  fi
  cargo build --release
  package_target ""
else
  # Build for each specified target
  for target in "${TARGETS[@]}"; do
    if [[ $VERBOSE -eq 1 ]]; then
      echo "Building for target: ${target}..."
    fi
    cargo build --release --target "${target}"
    package_target "${target}"
  done
fi

echo "Deployment complete!"
echo "Packages available in: ${DIST_DIR}"
ls -la "${DIST_DIR}"/*.tar.gz 2>/dev/null || echo "No tarballs created"