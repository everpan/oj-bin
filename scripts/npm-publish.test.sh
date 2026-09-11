#!/usr/bin/env bash
# npm-publish.sh 的 dry-run 自检：fixture 产物 → 断言装配布局/元数据/门禁。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
VER=$(awk -F'"' '/^version =[[:space:]]*"/ { print $2; exit }' oj/Cargo.toml)
T=x86_64-unknown-linux-gnu

# fixture：与 deploy.sh 产物同形（含顶层目录 + .sha256 干扰项）
SRC="$TMP/src/oj-v${VER}-${T}"
mkdir -p "$SRC/plugins/$T" "$SRC/devkit" "$TMP/dist"
echo fake-oj > "$SRC/oj"
echo fake-so > "$SRC/plugins/$T/libx.so"
echo fake-doc > "$SRC/devkit/api-manual.md"
tar -czf "$TMP/dist/oj-v${VER}-${T}.tar.gz" -C "$TMP/src" "oj-v${VER}-${T}"
echo sum > "$TMP/dist/oj-v${VER}-${T}.tar.gz.sha256"

echo "== case 1: dry-run 装配 =="
DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage" DRY_RUN=1 bash scripts/npm-publish.sh "v${VER}"

P="$TMP/stage/oj-$T"
[[ -f "$P/oj" ]]                       || { echo "FAIL: oj 未在包根（strip-components 失效）"; exit 1; }
[[ -f "$P/plugins/$T/libx.so" ]]       || { echo "FAIL: plugins 布局错"; exit 1; }
[[ -f "$P/devkit/api-manual.md" ]]     || { echo "FAIL: devkit 布局错"; exit 1; }
grep -q "\"name\": \"@oj-bin/oj-$T\"" "$P/package.json" || { echo "FAIL: name 注入错"; exit 1; }
grep -q "\"version\": \"$VER\""        "$P/package.json" || { echo "FAIL: version 注入错"; exit 1; }
grep -q '"linux"' "$P/package.json" && grep -q '"x64"' "$P/package.json" || { echo "FAIL: os/cpu 注入错"; exit 1; }
[[ -f "$P/README.md" ]] || { echo "FAIL: README 未拷入"; exit 1; }
grep -q "\"version\": \"$VER\"" "$TMP/stage/oj-main/package.json" || { echo "FAIL: 主包 version 注入错"; exit 1; }
[[ -f "$TMP/stage/oj-main/postinstall.js" ]] || { echo "FAIL: 主包缺 postinstall.js"; exit 1; }
echo "case 1 OK"

echo "== case 2: 未知 triple 门禁（musl 未入表 → 必须 fail）=="
M=x86_64-unknown-linux-musl
SRCM="$TMP/src/oj-v${VER}-${M}"
mkdir -p "$SRCM/plugins/$M" "$SRCM/devkit"
echo fake-oj > "$SRCM/oj"; echo fake > "$SRCM/plugins/$M/libx.so"; echo fake > "$SRCM/devkit/api-manual.md"
tar -czf "$TMP/dist/oj-v${VER}-${M}.tar.gz" -C "$TMP/src" "oj-v${VER}-${M}"
if DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage2" DRY_RUN=1 bash scripts/npm-publish.sh "v${VER}" 2>"$TMP/err"; then
  echo "FAIL: 未知 triple 未被门禁拦下"; exit 1
fi
grep -q "未知 triple" "$TMP/err" || { echo "FAIL: 门禁报错文案缺失"; cat "$TMP/err"; exit 1; }
echo "case 2 OK"

echo "== case 3: 版本门禁（tag != oj/Cargo.toml）=="
if DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage3" DRY_RUN=1 bash scripts/npm-publish.sh "v0.0.0-bogus" 2>"$TMP/err3"; then
  echo "FAIL: 版本不一致未被拦下"; exit 1
fi
grep -q "不一致" "$TMP/err3" || { echo "FAIL: 版本门禁文案缺失"; cat "$TMP/err3"; exit 1; }
echo "case 3 OK"
echo "ALL OK"
