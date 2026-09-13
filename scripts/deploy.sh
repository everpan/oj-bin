#!/usr/bin/env bash
set -euo pipefail

# 发行打包脚本（macOS / Linux）。Windows 用 scripts/deploy.bat。
# 构建（release）+ 打包 + 校验和，输入来自 cargo xtask 归置好的 bin/：
#   bin/oj                  -> 主程序
#   bin/plugins/<triple>/   -> 插件 cdylib
#   bin/devkit/             -> DevKit 文档
#
# 产物：dist/oj-v<version>-<host-triple>.tar.gz（+ 同名 .sha256）
# 包内根路径与包名同形：oj-v<version>-<host-triple>/{oj,oj.bin,lib/,plugins/<triple>/,devkit/}
#   - oj        : 启动器脚本（仅 Linux glibc 构建产物；macOS/Windows 为真实可执行文件）
#   - oj.bin    : 真实 ELF（被启动器调用）
#   - lib/      : 跟随发布的 glibc 运行时（仅 Linux glibc 构建产物）
#   macOS / Windows 包内无 oj.bin 与 lib/，oj / oj.exe 为真实可执行文件。
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

# === Linux (glibc) 跟随发布：打包自带 glibc 运行时，并用启动器绕开宿主 glibc ===
# 原理：把构建主机（或 CI 容器，见下）的 glibc 运行时连同真实二进制一起发布，由启动器
# 脚本显式调用自带 ld-linux 加载 oj.bin（--library-path 指向自带 lib/），运行期完全使用
# 自带 glibc，不受目标机 glibc 版本过低影响。因此「旧系统 glibc 太低无法运行」的问题消失。
# 前提：构建环境本身的 glibc 符号需求即产物下限。项目以 Dockerfile（ubuntu:20.04，glibc
# 2.31）锚定基线——在该容器内构建时，lib/ 自然收纳 2.31，内核 >= 3.2 即可运行；在 CI
# ubuntu-latest（glibc 2.39）上构建则收纳 2.39，内核需支持 2.39。本步骤对版本无假设，
# 仅「忠实打包构建环境的 glibc」。macOS / Windows 不走此分支。
if [[ "$TRIPLE" == *linux*gnu* ]]; then
  echo "Bundling glibc runtime for ${TRIPLE} ..."

  GLIBC_DIR="${TEMP_DIR}/lib"
  mkdir -p "${GLIBC_DIR}"

  # 真实二进制当前的 ELF 解释器（如 /lib64/ld-linux-x86-64.so.2）；其基名即自带
  # ld-linux 的文件名，启动器将显式 exec 它。
  INTERP="$(readelf -l "${BIN}" 2>/dev/null | sed -n -E "s/.*program interpreter: (.*)\].*/\1/p" | head -1)"
  if [[ -z "$INTERP" ]]; then
    INTERP="$(objdump -p "${BIN}" 2>/dev/null | awk '/INTERP/ {getline; print $1; exit}')"
  fi
  if [[ -z "$INTERP" ]]; then
    echo "Error: 无法从 ${BIN} 解析 ELF 解释器（需 binutils 的 readelf/objdump）" >&2
    exit 1
  fi
  INTERP_BASE="$(basename "$INTERP")"

  # 收集主程序 + 全部插件的系统级运行时依赖（ldd 解析后的真实路径），只保留位于系统
  # lib 树下的（排除 vdso / 本包自带插件），并复制进 lib/。这会自动涵盖 glibc 全套
  # （libc/libm/libpthread/libdl/librt/libresolv/libnsl/libutil/libcrypt/...）、GCC 运行时
  # （libgcc_s/libstdc++）以及任何被动态链接的系统 .so。
  echo "  collecting system shared libraries via ldd ..."
  SEEN=""
  while read -r p; do
    [[ -z "$p" || "$p" == linux-vdso.so.1 ]] && continue
    case "$p" in
      /lib/*|/usr/lib/*|/lib64/*|/usr/lib64/*) ;;
      *) continue ;;
    esac
    [[ -e "$p" ]] || continue
    base="$(basename "$p")"
    # 按文件名去重
    case " $SEEN " in *" $base "*) continue ;; esac
    SEEN="$SEEN $base"
    cp -L "$p" "${GLIBC_DIR}/"
  done < <(ldd "${BIN}" "${TRIPLE_DIR}"/*.so 2>/dev/null \
            | sed -n -E 's#.*=>[[:space:]]+(/[^[:space:]]+).*#\1#p; s#^[[:space:]]*(/[^[:space:]]+) \(0x[0-9a-f]+\)#\1#p')

  # 健壮性断言：至少要有 libc 与自带 ld-linux。
  if [[ ! -e "${GLIBC_DIR}/libc.so.6" ]]; then
    echo "Error: glibc 捆绑失败——lib/ 下缺少 libc.so.6" >&2
    exit 1
  fi
  if [[ ! -e "${GLIBC_DIR}/${INTERP_BASE}" ]]; then
    echo "Error: glibc 捆绑失败——lib/ 下缺少解释器 ${INTERP_BASE}" >&2
    exit 1
  fi

  # 可选 strip：减小包体（与 Dockerfile 一致）。失败不致命。
  if command -v strip >/dev/null 2>&1; then
    strip "${TEMP_DIR}/${BINARY_NAME}" "${TEMP_DIR}/plugins/${TRIPLE}"/*.so 2>/dev/null || true
  fi

  # 重命名真实二进制 -> oj.bin，并写入启动器 oj（脚本）。
  mv "${TEMP_DIR}/${BINARY_NAME}" "${TEMP_DIR}/${BINARY_NAME}.bin"
  cat > "${TEMP_DIR}/${BINARY_NAME}" <<EOF
#!/bin/sh
# oj-bin 自带 glibc 启动器（仅 Linux glibc 发行包）。直接调用打包的 ld-linux 加载
# 真实二进制 oj.bin，使其使用 lib/ 内的 glibc 运行时，彻底绕开宿主机 glibc 版本限制。
DIR="\$(cd "\$(dirname "\$(readlink -f "\$0")")" && pwd)"
exec "\$DIR/lib/${INTERP_BASE}" --library-path "\$DIR/lib" "\$DIR/${BINARY_NAME}.bin" "\$@"
EOF
  chmod +x "${TEMP_DIR}/${BINARY_NAME}"

  echo "  glibc bundled: $(ls -1 "${GLIBC_DIR}" | wc -l | tr -d ' ') files -> ${PACKAGE_NAME}/lib/"
fi

# 发布门禁（v0.1.12）：把「构建机才有的 JS 源」临时改名，用暂存二进制跑最小 `oj build`
# —— deno_core 0.411 的 dir 形式曾把扩展 JS 的绝对路径烧进二进制，非构建机上 JsRuntime
# 初始化即 ENOENT。任何残留的路径依赖都会在此失败；实现见 `cargo xtask smoke`。
echo "release gate: off-build-machine smoke ..."
if ! cargo xtask smoke --bin "${TEMP_DIR}/${BINARY_NAME}"; then
  echo "Error: 发布门禁失败——产物依赖构建机路径，不可发布" >&2
  exit 1
fi

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
