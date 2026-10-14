#!/usr/bin/env bash
set -euo pipefail

# 安装脚本：从 dist/ 取指定发行包，解压并覆盖安装到 ~/.local/share/oj。
# 包内根路径与包名同形：oj-v<version>-<host-triple>/{oj,oj.bin,lib/,plugins/<triple>/,devkit/}，
# 该根目录整体覆盖到安装目录。
#
# 用法：
#   scripts/install.sh                         # 自动选 dist/ 下 mtime 最新的 oj-v*.tar.gz
#   scripts/install.sh oj-v0.1.65-aarch64-apple-darwin        # 按包名（自动补 .tar.gz，在 dist/ 下找）
#   scripts/install.sh path/to/oj-v0.1.65-xxx.tar.gz          # 按 tar.gz 路径
#   scripts/install.sh <包> /custom/oj                        # 第二参数指定安装位置
#
# 安装目录可用环境变量覆盖：OJ_INSTALL_DIR=/opt/oj scripts/install.sh
# （第二位置参数优先级高于 OJ_INSTALL_DIR；显式指定的安装位置须已存在且为空，避免误覆盖含数据目录）

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${PROJECT_ROOT}/dist"
INSTALL_DIR="${OJ_INSTALL_DIR:-$HOME/.local/share/oj}"

ARG="${1:-}"
ARG2="${2:-}"

# 第二参数可显式指定安装位置（优先级高于 OJ_INSTALL_DIR），用于安全隔离安装。
if [[ -n "$ARG2" ]]; then
  INSTALL_DIR="$ARG2"
fi

# 解析出 tar.gz 归档路径
if [[ -z "$ARG" ]]; then
  # 自动选：dist/ 下 mtime 最新的 oj-v*.tar.gz
  shopt -s nullglob
  candidates=("${DIST_DIR}"/oj-v*.tar.gz)
  shopt -u nullglob
  if [[ ${#candidates[@]} -eq 0 ]]; then
    echo "Error: dist/ 下没有 oj-v*.tar.gz，请先运行 scripts/deploy.sh" >&2
    exit 1
  fi
  ARCHIVE="$(ls -t "${candidates[@]}" | head -1)"
elif [[ "$ARG" == *.tar.gz ]]; then
  if [[ -f "$ARG" ]]; then
    ARCHIVE="$ARG"
  elif [[ -f "${DIST_DIR}/$ARG" ]]; then
    ARCHIVE="${DIST_DIR}/$ARG"
  else
    echo "Error: 找不到 tar.gz：$ARG" >&2
    exit 1
  fi
else
  # 当作包名（不带 .tar.gz），在 dist/ 下找
  if [[ -f "${DIST_DIR}/${ARG}.tar.gz" ]]; then
    ARCHIVE="${DIST_DIR}/${ARG}.tar.gz"
  else
    echo "Error: dist/ 下找不到 ${ARG}.tar.gz" >&2
    exit 1
  fi
fi

PKG_NAME="$(basename "${ARCHIVE%.tar.gz}")"

echo "包   : ${ARCHIVE}"
echo "目标 : ${INSTALL_DIR}"

# 解压到临时目录（macOS BSD tar 会塞 ._* AppleDouble，关掉）
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
export COPYFILE_DISABLE=1
tar -xzf "${ARCHIVE}" -C "${TMP}"

SRC="${TMP}/${PKG_NAME}"
if [[ ! -d "$SRC" ]]; then
  echo "Error: 归档内缺少根目录 ${PKG_NAME}/" >&2
  exit 1
fi

# 显式指定的安装位置须已存在且为空（安全约束：不主动清空/删除，避免误覆盖含数据目录）。
if [[ -n "$ARG2" ]]; then
  if [[ ! -d "$INSTALL_DIR" ]]; then
    echo "Error: 安装位置不存在：${INSTALL_DIR}（须预先创建）" >&2
    exit 1
  fi
  shopt -s nullglob dotglob
  contents=("$INSTALL_DIR"/*)
  shopt -u nullglob dotglob
  if [[ ${#contents[@]} -gt 0 ]]; then
    echo "Error: 安装位置非空：${INSTALL_DIR}（须为空目录，脚本不会主动清空）" >&2
    exit 1
  fi
fi

# 覆盖安装：cp -R 合并——包内文件覆盖目标同名文件，目标独有文件保留。
# 若要彻底清掉旧安装（含已移除的插件/devkit 文件），先手动 `rm -rf "${INSTALL_DIR}"` 再跑本脚本。
mkdir -p "${INSTALL_DIR}"
cp -R "${SRC}/." "${INSTALL_DIR}/"

echo "已安装到 ${INSTALL_DIR}"
ls -la "${INSTALL_DIR}"
