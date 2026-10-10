#!/usr/bin/env bash
# npm 发布脚本（单一真相来源，CI 与本地同形）。
# 用法:   bash scripts/npm-publish.sh <tag>
# env:    DIST_DIR（默认 ./dist） STAGE_DIR（默认 mktemp） DRY_RUN=1（只装配+断言，不 publish）
# 顺序:   OIDC 预检 → 平台子包逐个 publish（任一真失败即死，绝不发主包）→ 主包 → 发布后置信断言。
# 兼容 bash 3.2（macOS 自带）：禁 declare -A / mapfile。
#
# 鉴权：只走 npm Trusted Publishing（OIDC，docs.npmjs.com/trusted-publishers）——
# 账号开启 2FA 后，非交互发布只剩两条路：bypass 2FA 的 granular token（长期凭证，要轮换，
# 且包级设了「disallow tokens」就彻底不可用）或 OIDC（无长期凭证，官方对 CI 的推荐解）。
# 本脚本选后者：凭证由 npm CLI 用 GitHub Actions 的 OIDC id-token 现换现用，
# 仓库里不再存 NPM_TOKEN，也就没有 token 泄漏/轮换问题。
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

# ---- 1b. OIDC 预检（Trusted Publishing 前置条件，先于一切装配动作）------
# 放到最前：OIDC 配错（缺 id-token 权限 / npm CLI 太老）在 npm publish 阶段才暴露的话，
# 已经解包装配过一轮，报错还容易被「registry lag」的 re-view 分支误吞成已发布。
ver_ge() { # <have> <want>：x.y.z 逐段数值比较（npm -v 形如 11.6.0）
  awk -v a="$1" -v b="$2" 'BEGIN{
    split(a,x,"."); split(b,y,".")
    for(i=1;i<=3;i++){ if((x[i]+0)>(y[i]+0)) exit 0; if((x[i]+0)<(y[i]+0)) exit 1 }
    exit 0 }'
}
NPM_MIN=11.5.1   # 官方硬要求：Trusted Publishing 需 npm CLI ≥11.5.1 / Node ≥22.14.0
npm_have=$(npm -v 2>/dev/null) || npm_have=""
if [[ -z "$npm_have" ]]; then
  echo "::error::拿不到 npm 版本（npm CLI 不可用）" >&2
  exit 1
fi
if ! ver_ge "$npm_have" "$NPM_MIN"; then
  echo "::error::npm CLI ${npm_have} < ${NPM_MIN}：Trusted Publishing（OIDC）要求 npm ≥ ${NPM_MIN}（Node ≥ 22.14.0）；CI 里把 setup-node 的 node-version 提到 24" >&2
  exit 1
fi
# GitHub Actions 的 OIDC 环境变量由 runner 在 id-token: write 下注入；缺失即代表权限没开
# 或不在 GitHub 托管 runner 上（self-hosted 不受 Trusted Publishing 支持）。
if [[ -z "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" || -z "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ]]; then
  echo "::error::未检测到 GitHub OIDC 环境（ACTIONS_ID_TOKEN_REQUEST_URL / ACTIONS_ID_TOKEN_REQUEST_TOKEN 为空）：本脚本只走 Trusted Publishing，需在 workflow 的 job 上加 'permissions: id-token: write'，且只能用 GitHub 托管 runner" >&2
  exit 1
fi
# 长期 token 存在时 npm CLI 会在 OIDC 失败后静默回退到它——那样 OIDC 配错永远发现不了，
# 等于白迁移。故直接拒绝。
if [[ -n "${NODE_AUTH_TOKEN:-}" ]]; then
  echo "::error::检测到 NODE_AUTH_TOKEN：OIDC-only 模式不带长期 token（npm 会在 OIDC 失败时静默回退到 token，配错就永远暴露不了）。请从 workflow 中移除该 env" >&2
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
  # 不注入任何 token：npm CLI 检测到 OIDC 环境后自行换短时发布凭证。
  # provenance 由 GitHub Actions 自动生成（公共仓库 + 公共包），无需 --provenance。
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
cp npm/oj/bin.js npm/oj/postinstall.js npm/README.md "$maindir/"
[[ -f LICENSE ]] && cp LICENSE "$maindir/" || true
publish_pkg "$maindir" "${SCOPE}/oj"

# ---- 8. 发布后置信（DRY_RUN 跳过）----------------------------------------
if [[ "${DRY_RUN:-0}" != "1" ]]; then
  for t in "${triples[@]}"; do
    read -r os cpu <<<"$(os_cpu_of "$t")"
    pkg="${SCOPE}/oj-${t}"
    # registry 传播延迟：retry 8 次、退避至 ~5 分钟（镜像 CDN 最坏分钟级）。注意 npm view --json
    # 在版本不存在时退出码非 0 且把 E404 错误对象打印到 stdout（非空）。必须按「退出码成功且非
    # error 对象」判定拿到真元数据——否则错误 JSON 会被当成「非空元数据」短路掉 retry，误报
    # cpu/os 断言失败。
    meta=""; tb=""
    for attempt in 1 2 3 4 5 6 7 8; do
      if meta=$(npm view "${pkg}@${VERSION}" os cpu --json 2>/dev/null) && \
         [[ -n "$meta" && "$meta" != *'"error"'* ]]; then
        tb=$(npm view "${pkg}@${VERSION}" dist.tarball 2>/dev/null || true)
        break
      fi
      meta=""
      [[ $attempt -lt 8 ]] && sleep $(( attempt * 15 > 60 ? 60 : attempt * 15 ))
    done
    if [[ -z "$meta" ]]; then
      # npm view 仍不可见（CLI 负缓存/镜像差异）：直连 registry packument 作第二意见
      reg="$(npm config get registry)"; reg="${reg%/}"
      enc="$(printf '%s' "$pkg" | sed 's|/|%2F|')"
      pack="$(curl -fsSL --max-time 30 "$reg/$enc" 2>/dev/null || true)"
      if [[ -n "$pack" ]] && printf '%s' "$pack" | grep -q "\"${VERSION}\""; then
        printf '%s' "$pack" | grep -q "\"$os\""  || { echo "::error::${pkg} packument os 断言失败（期望 $os）" >&2; exit 1; }
        printf '%s' "$pack" | grep -q "\"$cpu\"" || { echo "::error::${pkg} packument cpu 断言失败（期望 $cpu）" >&2; exit 1; }
        tb="$(printf '%s' "$pack" | node -e 'const p=JSON.parse(require("fs").readFileSync(0,"utf8"));const v=p.versions&&p.versions[process.argv[1]];process.stdout.write((v&&v.dist&&v.dist.tarball)||"")' "$VERSION")"
        echo "note: npm view 尚未可见，已用 registry packument 兜底验证 ${pkg}@${VERSION}"
      else
        echo "::error::${pkg}@${VERSION} 元数据获取失败（可能未发布或 registry 传播延迟）：npm view 与 packument 均不可见" >&2
        exit 1
      fi
    fi
    if [[ -n "$meta" ]]; then
      echo "$meta" | grep -q "$os" || { echo "::error::${pkg} os 断言失败（期望 $os）：$meta" >&2; exit 1; }
      echo "$meta" | grep -q "$cpu" || { echo "::error::${pkg} cpu 断言失败（期望 $cpu）：$meta" >&2; exit 1; }
    fi
    # tarball 文件清单断言（npm pack 条目带 package/ 前缀）
    [[ -n "$tb" ]] || { echo "::error::${pkg} 拿不到 dist.tarball" >&2; exit 1; }
    case "$os" in
      win32)  want='oj\.exe$|\.dll$' ;;
      darwin) want='package/oj$|\.dylib$' ;;
      *)      want='package/oj$|\.so$' ;;
    esac
    list=$(curl -fsSL "$tb" | tar -tzf -) || { echo "::error::${pkg} tarball 获取/解压清单失败" >&2; exit 1; }
    echo "$list" | grep -qE "$want" || { echo "::error::${pkg} tarball 文件清单断言失败" >&2; exit 1; }
    echo "verified ${pkg}@${VERSION} os=$os cpu=$cpu"
  done
fi

echo "npm publish done: ${SCOPE}/oj@${VERSION} + ${#triples[@]} platform packages"
