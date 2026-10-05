#!/usr/bin/env bash
# CI 自供给 sample 密钥对并重封 ldap 口令，使 `oj test` / `oj serve` 能在**无仓库私钥**
# 的 CI 克隆上正常解密 config.yaml 里的 ENC[...]。
#
# 背景：sample/config/keys/secrets-private.pem 被 gitignore（私钥不得进仓库）；而
# sample/config.yaml 里的 ldap.bind_pw 是用**已提交**的 secrets-public.pem 密封的。
# 缺失私钥时配置加载即 fail-fast（见 src/config.rs 的 sealed_value_without_private_key_fails_fast）。
# 本脚本生成一对匹配密钥（公钥顺手覆盖 CI checkout 内的 secrets-public.pem），用新公钥
# 重封 ldap 口令——L1 套件不触 LDAP，口令值无关，重点是 ENC[...] 在 CI 可被正常解密。
#
# 不向仓库写入任何私钥，符合「私钥不得进仓库」红线；仅改动 CI 运行期的 checkout 副本。
#
# 用法：bash scripts/ci-sample-secrets.sh <path-to-oj-binary>
set -euo pipefail

BIN="${1:?usage: ci-sample-secrets.sh <path-to-oj-binary>}"
DIR=sample/config/keys

mkdir -p "$DIR"
"$BIN" secret keygen --out-dir "$DIR" --force
ENC="$("$BIN" secret seal -k "$DIR/secrets-public.pem" <<< "ci-ldap-bind-pw")"

# 用新密文替换 config.yaml 里的 ENC[...]（保留行尾注释；base64url 不含 & \ ]，sed 安全）。
sed -i.bak -E "s|(bind_pw: )ENC\[[^]]*\]|\1${ENC}|" sample/config.yaml
rm -f sample/config.yaml.bak

echo "ci-sample-secrets: resealed ldap.bind_pw for CI (private key at $DIR/secrets-private.pem)"
