#!/usr/bin/env bash
# npm-publish.sh 的 dry-run 自检：fixture 产物 → 断言装配布局/元数据/门禁。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
VER=$(awk -F'"' '/^version =[[:space:]]*"/ { print $2; exit }' oj/Cargo.toml)
T=x86_64-unknown-linux-gnu

# 发布脚本只走 Trusted Publishing（OIDC）：dry-run 也要过 OIDC 预检，故注入假的 runner
# OIDC 环境变量；同时用假 npm 垫 PATH，让「npm 版本门禁」用例可复现、且不受 runner
# 自带 npm 版本影响（dry-run 下脚本只调 `npm -v`）。
OIDC=(ACTIONS_ID_TOKEN_REQUEST_URL=https://pipelines.actions.githubusercontent.com/fake \
      ACTIONS_ID_TOKEN_REQUEST_TOKEN=fake-token)
fake_npm() { # <dir> <version>
  mkdir -p "$1"
  printf '#!/bin/sh\necho %s\n' "$2" > "$1/npm"
  chmod +x "$1/npm"
}
fake_npm "$TMP/bin" 11.6.0
export PATH="$TMP/bin:$PATH"

# fixture：与 deploy.sh 产物同形（含顶层目录 + .sha256 干扰项）
SRC="$TMP/src/oj-v${VER}-${T}"
mkdir -p "$SRC/plugins/$T" "$SRC/devkit" "$TMP/dist"
echo fake-oj > "$SRC/oj"
echo fake-so > "$SRC/plugins/$T/libx.so"
echo fake-doc > "$SRC/devkit/api-manual.md"
tar -czf "$TMP/dist/oj-v${VER}-${T}.tar.gz" -C "$TMP/src" "oj-v${VER}-${T}"
echo sum > "$TMP/dist/oj-v${VER}-${T}.tar.gz.sha256"

echo "== case 1: dry-run 装配 =="
env "${OIDC[@]}" DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage" DRY_RUN=1 bash scripts/npm-publish.sh "v${VER}"

P="$TMP/stage/oj-$T"
[[ -f "$P/oj" ]]                       || { echo "FAIL: oj 未在包根（strip-components 失效）"; exit 1; }
[[ -f "$P/plugins/$T/libx.so" ]]       || { echo "FAIL: plugins 布局错"; exit 1; }
[[ -f "$P/devkit/api-manual.md" ]]     || { echo "FAIL: devkit 布局错"; exit 1; }
grep -q "\"name\": \"@oj-bin/oj-$T\"" "$P/package.json" || { echo "FAIL: name 注入错"; exit 1; }
grep -q "\"version\": \"$VER\""        "$P/package.json" || { echo "FAIL: version 注入错"; exit 1; }
grep -q '"linux"' "$P/package.json" && grep -q '"x64"' "$P/package.json" || { echo "FAIL: os/cpu 注入错"; exit 1; }
[[ -f "$P/README.md" ]] || { echo "FAIL: README 未拷入"; exit 1; }
# OIDC 强校验项：repository.url 必须与发布仓库（GitHub 仓库已从 only-js 改名 oj-bin）完全一致
grep -q "git+https://github.com/everpan/oj-bin.git" "$P/package.json" \
  || { echo "FAIL: 平台包 repository.url 与发布仓库不一致"; exit 1; }
grep -q "git+https://github.com/everpan/oj-bin.git" "$TMP/stage/oj-main/package.json" \
  || { echo "FAIL: 主包 repository.url 与发布仓库不一致"; exit 1; }
grep -q "\"version\": \"$VER\"" "$TMP/stage/oj-main/package.json" || { echo "FAIL: 主包 version 注入错"; exit 1; }
[[ -f "$TMP/stage/oj-main/postinstall.js" ]] || { echo "FAIL: 主包缺 postinstall.js"; exit 1; }
echo "case 1 OK"

echo "== case 2: 未知 triple 门禁（musl 未入表 → 必须 fail）=="
M=x86_64-unknown-linux-musl
SRCM="$TMP/src/oj-v${VER}-${M}"
mkdir -p "$SRCM/plugins/$M" "$SRCM/devkit"
echo fake-oj > "$SRCM/oj"; echo fake > "$SRCM/plugins/$M/libx.so"; echo fake > "$SRCM/devkit/api-manual.md"
tar -czf "$TMP/dist/oj-v${VER}-${M}.tar.gz" -C "$TMP/src" "oj-v${VER}-${M}"
if env "${OIDC[@]}" DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage2" DRY_RUN=1 bash scripts/npm-publish.sh "v${VER}" 2>"$TMP/err"; then
  echo "FAIL: 未知 triple 未被门禁拦下"; exit 1
fi
grep -q "未知 triple" "$TMP/err" || { echo "FAIL: 门禁报错文案缺失"; cat "$TMP/err"; exit 1; }
echo "case 2 OK"

echo "== case 3: 版本门禁（tag != oj/Cargo.toml）=="
if env "${OIDC[@]}" DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage3" DRY_RUN=1 bash scripts/npm-publish.sh "v0.0.0-bogus" 2>"$TMP/err3"; then
  echo "FAIL: 版本不一致未被拦下"; exit 1
fi
grep -q "不一致" "$TMP/err3" || { echo "FAIL: 版本门禁文案缺失"; cat "$TMP/err3"; exit 1; }
echo "case 3 OK"

echo "== case 4: OIDC 预检（无 GitHub OIDC 环境 → 必须 fail）=="
# 官方要求：Trusted Publishing 只在 GitHub 托管 runner + id-token: write 下成立。
# 缺 id-token 权限时 runner 不注入这两个变量——必须在动装配之前就死，而不是等 npm publish 401。
if env -u ACTIONS_ID_TOKEN_REQUEST_URL -u ACTIONS_ID_TOKEN_REQUEST_TOKEN \
     DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage4" DRY_RUN=1 \
     bash scripts/npm-publish.sh "v${VER}" 2>"$TMP/err4"; then
  echo "FAIL: 缺 OIDC 环境未被拦下"; exit 1
fi
grep -q "未检测到 GitHub OIDC 环境" "$TMP/err4" || { echo "FAIL: OIDC 门禁文案缺失"; cat "$TMP/err4"; exit 1; }
[[ ! -d "$TMP/stage4" ]] || { echo "FAIL: OIDC 门禁应在装配前拦下（stage4 不该产生）"; exit 1; }
echo "case 4 OK"

echo "== case 5: 长期 token 拒绝（NODE_AUTH_TOKEN 存在 → 必须 fail）=="
# npm CLI 在 OIDC 失败时会静默回退到 token，那样配错永远发现不了 → 直接拒绝。
if env "${OIDC[@]}" NODE_AUTH_TOKEN=fake \
     DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage5" DRY_RUN=1 \
     bash scripts/npm-publish.sh "v${VER}" 2>"$TMP/err5"; then
  echo "FAIL: NODE_AUTH_TOKEN 未被拒绝"; exit 1
fi
grep -q "NODE_AUTH_TOKEN" "$TMP/err5" || { echo "FAIL: token 拒绝文案缺失"; cat "$TMP/err5"; exit 1; }
echo "case 5 OK"

echo "== case 6: npm CLI 版本门禁（10.9.0 < 11.5.1 → 必须 fail）=="
fake_npm "$TMP/bin-old" 10.9.0
if env "${OIDC[@]}" PATH="$TMP/bin-old:$PATH" \
     DIST_DIR="$TMP/dist" STAGE_DIR="$TMP/stage6" DRY_RUN=1 \
     bash scripts/npm-publish.sh "v${VER}" 2>"$TMP/err6"; then
  echo "FAIL: npm 版本过低未被拦下"; exit 1
fi
grep -q "npm CLI 10.9.0" "$TMP/err6" || { echo "FAIL: npm 版本门禁文案缺失"; cat "$TMP/err6"; exit 1; }
echo "case 6 OK"

echo "== case 7: npm CLI 边界（恰好 11.5.1 → 放行）=="
fake_npm "$TMP/bin-edge" 11.5.1
# 用只含 gnu 产物的干净 dist（case 2 往 $TMP/dist 塞了 musl，会被 triple 门禁拦下）
mkdir -p "$TMP/dist7"; cp "$TMP/dist/oj-v${VER}-${T}.tar.gz" "$TMP/dist7/"
if ! env "${OIDC[@]}" PATH="$TMP/bin-edge:$PATH" \
     DIST_DIR="$TMP/dist7" STAGE_DIR="$TMP/stage7" DRY_RUN=1 \
     bash scripts/npm-publish.sh "v${VER}" >"$TMP/out7" 2>&1; then
  echo "FAIL: npm 11.5.1 应通过门禁"; cat "$TMP/out7"; exit 1
fi
echo "case 7 OK"
echo "ALL OK"
