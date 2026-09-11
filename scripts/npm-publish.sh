#!/usr/bin/env bash
# npm 发布脚本（单一真相来源，CI 与本地同形）。
# 用法:   bash scripts/npm-publish.sh <tag>
# env:    DIST_DIR（默认 ./dist） STAGE_DIR（默认 mktemp） DRY_RUN=1（只装配+断言，不 publish）
# 顺序:   平台子包逐个 publish（任一真失败即死，绝不发主包）→ 主包 → 发布后置信断言。
# 兼容 bash 3.2（macOS 自带）：禁 declare -A / mapfile。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

TAG="${1:?usage: npm-publish.sh <tag>}"
VERSION="${TAG#v}"   # npm version 不允许前导 v
SCOPE="@oj-bin"
DIST_DIR="${DIST_DIR:-$PWD/dist}"

# ---- 1. 版本一致性门禁（与 release.yml resolve-tag 同源，双保险）--------
cargo_version=$(awk -F'"' '/^version =[[:space:]]*"/ { print $2; exit }' oj/Cargo.toml)
if [[ "$VERSION" != "$cargo_version" ]]; then
  echo "::error::tag '${TAG}' 与 oj/Cargo.toml version '${cargo_version}' 不一致" >&2
  exit 1
fi

# ---- 2. triple → os cpu（与 npm/oj/postinstall.js 的 TRIPLES 反向表交叉维护：改一边必须改另一边）
os_cpu_of() {
  case "$1" in
    x86_64-unknown-linux-gnu)  echo "linux x64" ;;
    aarch64-apple-darwin)      echo "darwin arm64" ;;
    x86_64-pc-windows-msvc)    echo "win32 x64" ;;
    *) return 1 ;;
  esac
}

# ---- 3. 收集产物 triple（显式后缀 glob，排除 .sha256）-------------------
triples=()
for f in "$DIST_DIR"/oj-v*-*.tar.gz "$DIST_DIR"/oj-v*-*.zip; do
  [[ -e "$f" ]] || continue
  base=$(basename "$f"); base="${base%.tar.gz}"; base="${base%.zip}"
  triples+=("${base#oj-v${VERSION}-}")
done
[[ ${#triples[@]} -gt 0 ]] || { echo "::error::${DIST_DIR} 下无 oj-v${VERSION}-* 产物" >&2; exit 1; }

# ---- 4. 防呆：未知 triple / 同 (os,cpu) 撞车（musl 与 gnu 并存时撞 linux+x64——
#         npm os/cpu 无法区分，启用 musl 前必须先定 libc 策略）--------------
seen=""
for t in "${triples[@]}"; do
  oscpu=$(os_cpu_of "$t") || { echo "::error::未知 triple '$t'（os_cpu_of 未覆盖，先扩映射表再发布）" >&2; exit 1; }
  case " $seen " in
    *" $oscpu "*) echo "::error::(os,cpu)=[$oscpu] 撞车：${t}——同一 (os,cpu) 不允许两个 triple" >&2; exit 1 ;;
  esac
  seen="$seen $oscpu"
done

# ---- 5. publish-first 幂等 ----------------------------------------------
# npm view 预检命中 CDN 旧缓存可能误判不存在 → publish 失败后 re-view，可见即成功。
publish_pkg() { # <pkgdir> <pkg-name>
  local dir="$1" pkg="$2" out
  if [[ "${DRY_RUN:-0}" == "1" ]]; then echo "[dry-run] publish ${pkg}@${VERSION} ($dir)"; return 0; fi
  if npm view "${pkg}@${VERSION}" version >/dev/null 2>&1; then
    echo "skip ${pkg}@${VERSION} (already published)"; return 0
  fi
  if ! out=$(npm publish "$dir" --access public 2>&1); then
    if npm view "${pkg}@${VERSION}" version >/dev/null 2>&1; then
      echo "already published (registry lag), skip ${pkg}@${VERSION}"
    else
      echo "$out" >&2; echo "::error::publish ${pkg}@${VERSION} 失败" >&2; exit 1
    fi
  else
    echo "published ${pkg}@${VERSION}"
  fi
}

STAGE_DIR="${STAGE_DIR:-$(mktemp -d)}"

# ---- 6. 装配 + 发布平台子包（失败即 exit，绝不进第 7 步发主包）-----------
for t in "${triples[@]}"; do
  read -r os cpu <<<"$(os_cpu_of "$t")"
  pkgdir="$STAGE_DIR/oj-$t"
  mkdir -p "$pkgdir"
  if [[ -f "$DIST_DIR/oj-v${VERSION}-${t}.tar.gz" ]]; then
    # 剥掉归档内顶层 oj-v<ver>-<triple>/ 目录（GNU/BSD tar 均支持）
    tar -xzf "$DIST_DIR/oj-v${VERSION}-${t}.tar.gz" -C "$pkgdir" --strip-components=1
  else
    # deploy.bat 用 bsdtar 产 zip：python3 zipfile 兼容性最稳，unzip 兜底
    if command -v python3 >/dev/null 2>&1; then
      python3 -m zipfile -e "$DIST_DIR/oj-v${VERSION}-${t}.zip" "$pkgdir/.x"
    else
      unzip -q "$DIST_DIR/oj-v${VERSION}-${t}.zip" -d "$pkgdir/.x"
    fi
    mv "$pkgdir/.x/oj-v${VERSION}-${t}/"* "$pkgdir/"
    rm -rf "$pkgdir/.x"
  fi
  # 布局断言：包根必须直接是 oj[.exe] / plugins/<triple>/ / devkit/
  [[ -f "$pkgdir/oj" || -f "$pkgdir/oj.exe" ]] || { echo "::error::$t 包根缺 oj 二进制（strip 失效？）" >&2; exit 1; }
  [[ -d "$pkgdir/plugins/$t" ]] || { echo "::error::$t 缺 plugins/$t" >&2; exit 1; }
  [[ -f "$pkgdir/devkit/api-manual.md" ]] || { echo "::error::$t 缺 devkit" >&2; exit 1; }
  sed -e "s/__TRIPLE__/$t/g" -e "s/__VERSION__/$VERSION/g" \
      -e "s/__OS__/$os/g" -e "s/__CPU__/$cpu/g" \
      npm/platform/package.json > "$pkgdir/package.json"
  cp npm/README.md "$pkgdir/README.md"
  [[ -f LICENSE ]] && cp LICENSE "$pkgdir/" || true
  publish_pkg "$pkgdir" "${SCOPE}/oj-${t}"
done

# ---- 7. 装配 + 发布主包 ---------------------------------------------------
# 模板 optionalDependencies 与实际产物 triple 集必须互为充要（防模板/矩阵漂移）
maindir="$STAGE_DIR/oj-main"
mkdir -p "$maindir"
for t in "${triples[@]}"; do
  grep -q "${SCOPE}/oj-${t}" npm/oj/package.json || { echo "::error::主包模板 optionalDependencies 缺 $t" >&2; exit 1; }
done
for name in $(grep -o "${SCOPE}/oj-[a-z0-9_-]*" npm/oj/package.json | sort -u); do
  t="${name#${SCOPE}/oj-}"
  ok=0
  for have in "${triples[@]}"; do [[ "$have" == "$t" ]] && ok=1; done
  if [[ $ok != 1 ]]; then
    if [[ "${DRY_RUN:-0}" == "1" ]]; then
      echo "[dry-run] skip template dep $name (no dist artifact)" >&2
    else
      echo "::error::主包模板列了 $name 但 dist/ 无对应产物" >&2; exit 1
    fi
  fi
done
sed "s/__VERSION__/$VERSION/g" npm/oj/package.json > "$maindir/package.json"
cp npm/oj/postinstall.js npm/README.md "$maindir/"
[[ -f LICENSE ]] && cp LICENSE "$maindir/" || true
publish_pkg "$maindir" "${SCOPE}/oj"

# ---- 8. 发布后置信（DRY_RUN 跳过）----------------------------------------
if [[ "${DRY_RUN:-0}" != "1" ]]; then
  for t in "${triples[@]}"; do
    read -r os cpu <<<"$(os_cpu_of "$t")"
    pkg="${SCOPE}/oj-${t}"
    # registry 传播延迟：retry 3 次
    meta=""
    for _ in 1 2 3; do
      meta=$(npm view "${pkg}@${VERSION}" os cpu --json 2>/dev/null) && [[ -n "$meta" ]] && break
      sleep 10
    done
    echo "$meta" | grep -q "$os" || { echo "::error::${pkg} os 断言失败（期望 $os）：$meta" >&2; exit 1; }
    echo "$meta" | grep -q "$cpu" || { echo "::error::${pkg} cpu 断言失败（期望 $cpu）：$meta" >&2; exit 1; }
    # tarball 文件清单断言（npm pack 条目带 package/ 前缀）
    tb=$(npm view "${pkg}@${VERSION}" dist.tarball)
    case "$os" in
      win32)  want='oj\.exe$|\.dll$' ;;
      darwin) want='package/oj$|\.dylib$' ;;
      *)      want='package/oj$|\.so$' ;;
    esac
    curl -fsSL "$tb" | tar -tzf - | grep -qE "$want" || { echo "::error::${pkg} tarball 文件清单断言失败" >&2; exit 1; }
    echo "verified ${pkg}@${VERSION} os=$os cpu=$cpu"
  done
fi

echo "npm publish done: ${SCOPE}/oj@${VERSION} + ${#triples[@]} platform packages"
