#!/usr/bin/env bash
# ============================================================================
# MSVC 构建环境（Git Bash / MSYS2 / Cygwin）。cmd.exe 用 scripts/windows-msvc-env.bat。
#
# 为什么需要这个
#   oj-bus-kafka 依赖 rdkafka → rdkafka-sys，后者在 Windows 上经 CMake 从源码编
#   librdkafka（feature `cmake-build` 只在 Windows 启用）。以下三件事在裸机上必失败：
#
#     1) CMake Error: Could not create named generator Visual Studio 18 2026
#        rdkafka-sys 探测到 VS 后硬编码 `-G "Visual Studio <版本>"`；本机 cmake
#        早于该生成器名就不认，直接 abort。钉 NMake 生成器即与 VS 版本/名字解耦。
#
#     2) CRT 混链（LNK4098 / __imp__* 悬空）
#        librdkafka 默认 /MD（动态 CRT），而 .cargo/config.toml 给 windows-msvc
#        设了 +crt-static（/MT）。cl.exe 把 `_CL_` 追加在命令行末尾，同类别旗标
#        后者胜，故 `_CL_=-MT` 必然压过 cmake 的 /MD。**必须 dash 形式**：slash
#        形式 `/MT` 会被 MSYS 当 POSIX 绝对路径改写成 `C:/Program Files/Git/MT`。
#
#     3) Git for Windows 自带 GNU `link.exe`（usr\bin）遮蔽 MSVC 链接器
#        → "missing operand after ..."。
#
#   这套步骤此前**只存在于 CI**（.github/workflows/plugin-matrix.yml 与 release.yml），
#   本地 Windows 构建因此失败且毫无线索。本脚本是它的本地对应物。
#
# 用法
#   source scripts/windows-msvc-env.sh      # 配置当前 shell（推荐）
#   scripts/windows-msvc-env.sh <命令>      # 在配置好的环境里跑命令
#
#   之后照常：
#     cargo xtask build
#     cargo test --workspace --release
#
# 注意：必须 `source`（或 `.`）才能把环境变量留在当前 shell。直接执行只影响子进程。
# ============================================================================
set -euo pipefail

# --- 环境自检 ---------------------------------------------------------------
case "${OSTYPE:-$(uname -s)}" in
    msys*|cygwin*|win32*) ;;
    *)
        echo "错误：本脚本仅供 Windows 的 Git Bash / MSYS2 / Cygwin 使用。" >&2
        echo "      当前平台：$(uname -s)。Linux / macOS 无需任何额外设置。" >&2
        exit 1
        ;;
esac

# --- 1. 定位 vcvars64.bat（MSVC 工具链 cl.exe / nmake.exe） -----------------
# vswhere 是官方推荐入口（VS 2017+ 自带）。cmd.exe 用 `where`；这里在 bash 里
# 用 Windows 的 `where.exe` 显式调用，避免 Git Bash 的 `where` 内建干扰。
_vswhere="$(command -v where.exe >/dev/null 2>&1 && where.exe vswhere.exe 2>/dev/null | tr -d '\r' | head -1 || true)"

_vcvars=""
if [[ -n "${_vswhere:-}" && -f "$_vswhere" ]]; then
    _installdir="$("$_vswhere" -latest -products '*' \
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 \
        -property installationPath 2>/dev/null | tr -d '\r' | head -1 || true)"
    if [[ -n "$_installdir" && -f "$_installdir/VC/Auxiliary/Build/vcvars64.bat" ]]; then
        _vcvars="$_installdir/VC/Auxiliary/Build/vcvars64.bat"
    fi
fi

# 回退：遍历常见安装路径（覆盖无 vswhere 或非常规布局的安装）。
# VS 2026 主版本号为 18；更早的版本用年份。
if [[ -z "$_vcvars" ]]; then
    for _v in 18 2026 2022 2019; do
        for _e in Community Professional Enterprise BuildTools; do
            _cand="/c/Program Files/Microsoft Visual Studio/$_v/$_e/VC/Auxiliary/Build/vcvars64.bat"
            if [[ -f "$_cand" ]]; then
                _vcvars="$_cand"
                break 2
            fi
        done
    done
fi

if [[ -z "$_vcvars" ]]; then
    cat >&2 <<'EOF'
错误：找不到 vcvars64.bat。

  已尝试 vswhere 与常见 Visual Studio 安装路径。
  请安装「使用 C++ 的桌面开发」工作负载（提供 cl.exe / nmake.exe），
  或改从 VS 开发者命令行运行。

EOF
    exit 1
fi
echo "[env] MSVC toolchain: $_vcvars"

# --- 2. 导入 MSVC 环境 ------------------------------------------------------
# vcvars64.bat 是 cmd 批处理，无法直接在 bash 里 source。标准桥接法：让 cmd.exe
# 跑 vcvars 后再 `set`，把全部变量以 KEY=VALUE 形式回传，在 bash 里逐条 export。
# 这样 PATH/INCLUDE/LIB/LIBPATH/WindowsSdkDir 等几十个变量一次性带过来——比手工
# 枚举可靠得多（手工枚举正是容易出错的地方）。
_win_vcvars="$(cygpath -w "$_vcvars" 2>/dev/null || echo "$_vcvars")"
_exported="$(
    MSYS2_ARG_CONV_EXCL='*' cmd.exe /c "call \"$_win_vcvars\" >nul 2>nul && set" 2>/dev/null \
        | tr -d '\r' \
        | grep -E '^[A-Za-z_][A-Za-z0-9_()]*=' || true
)"
if [[ -z "$_exported" ]]; then
    echo "错误：vcvars64.bat 未产出环境（桥接 cmd.exe 失败）。" >&2
    exit 1
fi

while IFS='=' read -r _k _v; do
    [[ -z "$_k" ]] && continue
    # 只接受合法 shell 标识符：VC 环境里还有像 "ProgramFiles(x86)" 这种名字，
    # 以及含空格/点号的杂项；export 不了的一律跳过（PATH/INCLUDE/LIB 等关键项
    # 都是纯标识符，不受影响）。
    case "$_k" in
        [A-Za-z_]*)
            case "$_k" in
                *[!A-Za-z0-9_]*) continue ;;
            esac
            ;;
        *) continue ;;
    esac
    export "$_k=$_v" 2>/dev/null || true
done <<<"$_exported"

# --- 3. 钉 CMake 生成器（不覆盖用户显式值） --------------------------------
if [[ -z "${CMAKE_GENERATOR:-}" ]]; then
    export CMAKE_GENERATOR="NMake Makefiles"
    echo "[env] CMAKE_GENERATOR=NMake Makefiles"
else
    echo "[env] CMAKE_GENERATOR 已是 \"$CMAKE_GENERATOR\" —— 保持不动"
fi

# --- 4. librdkafka 强制静态 CRT --------------------------------------------
if [[ -z "${_CL_:-}" ]]; then
    export _CL_="-MT"
    echo "[env] _CL_=-MT"
else
    echo "[env] _CL_ 已是 \"$_CL_\" —— 保持不动"
fi

# --- 5. 移开 Git for Windows 的 GNU link.exe -------------------------------
# Git 的 usr/bin/link.exe 在 PATH 中先于 MSVC 的，rustc 会选错。PATH 顺序继承自
# 父进程、无法在环境层修正，只能动文件。改名而非删除：这是用户自己的 Git 安装，
# 且在仓库之外。恢复：mv link.exe.disabled link.exe
for _g in \
    "/c/Program Files/Git/usr/bin/link.exe" \
    "/c/Program Files (x86)/Git/usr/bin/link.exe" \
    "$LOCALAPPDATA/Programs/Git/usr/bin/link.exe"
do
    if [[ -f "$_g" ]]; then
        if mv -f "$_g" "$_g.disabled" 2>/dev/null; then
            echo "[env] 已移开遮蔽的链接器：$_g.disabled"
        else
            echo "警告：无法移开 $_g（被占用 / 只读）" >&2
        fi
    fi
done

# --- 6. 跑命令（或提示后续步骤） -------------------------------------------
if [[ $# -gt 0 ]]; then
    echo "[env] running: $*"
    echo
    exec "$@"
fi

cat <<'EOF'

Windows MSVC 构建环境已就绪。后续：
  cargo xtask build            # oj + 全部插件 -> bin\
  cargo build --release        # workspace
  cargo test --workspace --release

（若你是直接执行而非 `source` 本脚本，上面的变量只在子进程里有效。）
EOF
