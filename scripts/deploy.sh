#!/usr/bin/env bash
set -euo pipefail

# 发行打包脚本（macOS / Linux）。Windows 用 scripts/deploy.ps1。
# 构建（release）+ 打包 + 校验和，输入来自 cargo xtask 归置好的 bin/：
#   bin/oj                  -> 主程序
#   bin/plugins/<triple>/   -> 插件 cdylib
#   bin/devkit/             -> DevKit 文档
#
# 产物：dist/oj-v<version>-<host-triple>.tar.gz（+ 同名 .sha256）
# 包内根路径与包名同形：oj-v<version>-<host-triple>/{oj,plugins/<triple>/,devkit/}
#
# 平台 triple 取自 `rustc -vV`，与 xtask 归置插件目录所用的 triple 同源
# （tools/xtask/src/main.rs 的 host_triple）——不写死平台判断，musl / aarch64
# 等变体天然可区分。
#
# 包内 plugins/<triple>/ 与插件加载器的发现路径同形（<exe>/plugins/<triple>/），
# 解包即可用，无需手工改目录名。

# macOS 的 BSD tar 会往归档里塞 ._* AppleDouble 文件，关掉。
export COPYFILE_DISABLE=1

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# xtask 是 .cargo/config.toml 里的 alias，必须在工作区内执行才生效。
cd "${PROJECT_ROOT}"

DIST_DIR="${PROJECT_ROOT}/dist"
BINARY_NAME="oj"

# 版本：oj/Cargo.toml 首个 `version = "..."`。用 awk 而非 grep+cut，
# 兼容 BSD awk（macOS 自带 bash 3.2 环境）与 gawk（Linux）。
VERSION=$(awk -F'"' '/^version =[[:space:]]*"/ { print $2; exit }' oj/Cargo.toml)
if [[ -z "$VERSION" ]]; then
  echo "Error: 无法从 oj/Cargo.toml 解析 version" >&2
  exit 1
fi

# 先整体取出再解析，不走管道：awk 提前 exit 会关掉管道写端，rustc 收到 SIGPIPE
# 而死（141），在 `set -o pipefail` 下会让整条赋值失败、脚本静默退出。
RUSTC_INFO="$(rustc -vV)"
TRIPLE="$(awk '/^host: /{ print $2; exit }' <<<"$RUSTC_INFO")"
if [[ -z "$TRIPLE" ]]; then
  echo "Error: 无法从 rustc -vV 解析 host triple" >&2
  exit 1
fi

PACKAGE_NAME="oj-v${VERSION}-${TRIPLE}"
TEMP_DIR="${DIST_DIR}/${PACKAGE_NAME}"

echo "host triple : ${TRIPLE}"
echo "version     : ${VERSION}"

# 清空 dist/（不删整个 dist 再建，避免并发时闪断）
rm -rf "${DIST_DIR}"
mkdir -p "${DIST_DIR}"

# 构建并归置 oj + 全部第一方插件 + devkit -> bin/
echo "Building release (oj + plugins + devkit) into bin/ ..."
cargo xtask build

# 校验产物
BIN="${PROJECT_ROOT}/bin/${BINARY_NAME}"
if [[ ! -x "$BIN" ]]; then
  echo "Error: 主程序缺失或不可执行：${BIN}" >&2
  exit 1
fi

TRIPLE_DIR="${PROJECT_ROOT}/bin/plugins/${TRIPLE}"
if [[ ! -d "$TRIPLE_DIR" ]]; then
  echo "Error: 插件目录缺失：${TRIPLE_DIR}" >&2
  echo "       bin/plugins/ 现有：$(ls -1 "${PROJECT_ROOT}/bin/plugins" 2>/dev/null | tr '\n' ' ')" >&2
  exit 1
fi

DEVKIT="${PROJECT_ROOT}/bin/devkit"
if [[ ! -f "${DEVKIT}/api-manual.md" || ! -f "${DEVKIT}/global.d.ts" ]]; then
  echo "Error: devkit 产物缺失：${DEVKIT}（run: cargo xtask build）" >&2
  exit 1
fi

# 装配：oj + plugins/<triple>/ + devkit/
mkdir -p "${TEMP_DIR}/plugins" "${TEMP_DIR}/devkit"
cp "${BIN}" "${TEMP_DIR}/${BINARY_NAME}"
chmod +x "${TEMP_DIR}/${BINARY_NAME}"
# 目标目录先建好再 cp -R：BSD cp 与 GNU cp 在「目标已存在」时都会落到
# 目标/<源基名>，行为一致；反过来（目标不存在）两者语义会分叉。
cp -R "${TRIPLE_DIR}" "${TEMP_DIR}/plugins/"
cp -R "${DEVKIT}/." "${TEMP_DIR}/devkit/"

# 打包
ARCHIVE_NAME="${PACKAGE_NAME}.tar.gz"
ARCHIVE="${DIST_DIR}/${ARCHIVE_NAME}"
tar -czf "${ARCHIVE}" -C "${DIST_DIR}" "${PACKAGE_NAME}"
rm -rf "${TEMP_DIR}"

# 校验和：Linux 用 sha256sum，macOS 只有 shasum。
if command -v sha256sum >/dev/null 2>&1; then
  (cd "${DIST_DIR}" && sha256sum "${ARCHIVE_NAME}" >"${ARCHIVE_NAME}.sha256")
else
  (cd "${DIST_DIR}" && shasum -a 256 "${ARCHIVE_NAME}" >"${ARCHIVE_NAME}.sha256")
fi

echo "Deployment complete!"
echo "Package: ${ARCHIVE}"
ls -la "${ARCHIVE}"
