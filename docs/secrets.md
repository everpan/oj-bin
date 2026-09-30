# 配置凭据密封手册（`ENC[...]`）

> 给谁读：要把 `config.yaml` 进 git / 进镜像 / 发工单附件的人，以及运维密钥的人。
> 快速上手看 `docs/devkit/api-manual.md` §10「secrets」；可照抄的场景看
> `docs/devkit/scenarios.md` 场景 18；本手册是完整参考（威胁模型、密文格式、命令、
> 迁移与轮换、限制、排障）。

- 版本：v0.1.33 起；**下一版（未发布）移除 RSA(v1) 信封，仅保留 X25519 信封**（config 不再有「RSA 还是 X25519」的歧义）。
- 实现：`src/secret.rs`（加解密 + 配置树解密 + 脱敏）+ `oj/src/secret_cmd.rs`（CLI）。
- 依赖：`x25519-dalek 3` + `hkdf 0.12` + `aes-gcm 0.10`（纯 Rust，无 C 编译）；随机数走
  `getrandom 0.2`。`rsa` 仍被 `oj-cert` / OIDC 使用，但 secrets 模块已不再依赖它。

---

## 1. 威胁模型：能防什么、不防什么

| | 说明 |
|---|---|
| ✅ 能防 | **配置文件泄漏**：误提交 git、镜像层、备份、工单附件。泄漏者拿到 `config.yaml` 也解不出密码 |
| ❌ 不防 | **私钥泄漏**——那等于全盘失守 |
| ❌ 不防 | **内存取证 / 进程 dump**——明文必然在内存里（要连库就得有明文） |
| ❌ 不防 | **有权读私钥的人**——这是「授信运维」，不是加密能解决的 |

**真正的收益**是两点：
1. 把「N 个密码的暴露面」收敛成「1 个私钥的暴露面」；
2. **加密权与解密权分离**——公钥可进仓库（开发者/CI 只加密），私钥只在部署机。

对比：对称主密钥（单一 KEK）做不到第 2 点，加密方也必须持有解密能力。

---

## 2. 密文格式

配置里写成字符串 `ENC[<base64url>]`，内部是**信封**（hybrid encryption）：

```
信封 = [0x02 版本][0x01 算法][X25519 临时公钥 32B][nonce 12B][AES-256-GCM(明文) + tag]
```

- **算法**：X25519 ECDH（临时私钥 × 收件方公钥）+ HKDF-SHA256 派生 32 字节会话密钥，
  正文由 AES-256-GCM 加密。**固定地板仅约 62B**（版本 1 + 算法 1 + 临时公钥 32 +
  nonce 12 + tag 16），密文长度≈明文+62B——8 字密码密文约 95 字符，短密码不再被 RSA 地板撑大。
- **为什么是信封（KEM + AES-GCM）而非直接用 X25519 加密**：X25519 本身只能做密钥协商、
  不能直接加密长明文；信封用 X25519 派生一次性会话密钥，正文交给 AES-256-GCM——后者
  明文长度无上限且带完整性校验。
- **为什么用 AES-GCM 而不是 CBC/裸流密码**：自带完整性校验——密文被改一个 bit 会
  **解密失败**，而不是解出一段垃圾密码后连库失败（后者极难定位）。
- base64url（无 `+`/`/`/`=`）：在 YAML 任何上下文都无需加引号，避免 `[]` 被当流序列。
- 随机 nonce + 临时密钥：同一明文两次加密结果不同（无法比对两个密文是否同源）。
- **版本字节**：首字节 `0x02` 标识本信封；旧版 v1（RSA-OAEP）已移除，遇到 v1 密文解密时
  明确报错、提示用 `oj secret seal` 以 X25519 重新加密，不静默降级。
- **RSA(v1) 已移除**：`oj secret keygen` 现在只生成 X25519 密钥，配置里只有一种密钥、
  不再有「RSA 还是 X25519」的歧义。

---

## 3. 命令

```bash
# ① 生成密钥对（X25519；私钥自动 0o600，已存在则拒绝覆盖）
./bin/oj secret keygen --out-dir keys
echo 'keys/secrets-private.pem' >> .gitignore     # 私钥绝不进仓库

# ② 加密（走 stdin；命令行参数会进 shell history 与 ps）
echo -n 'mysql://root:hunter2@127.0.0.1:3306/app' | ./bin/oj secret seal -k keys/secrets-public.pem
# → ENC[Ab3…]（X25519 信封，密文长度≈明文+62B）

# ③ 排障解密（走与启动同一条私钥通道）
./bin/oj secret open -c config.yaml 'ENC[Ab3…]'
```

| 子命令 | 关键参数 | 说明 |
|---|---|---|
| `keygen` | `--out-dir`、`--force` | 产出 `secrets-private.pem` / `secrets-public.pem`（X25519，32 字节，`BEGIN OJ X25519 …` 标签） |
| `seal` | `-k <pub.pem>` 或 `-c <config.yaml>`（取 `secrets.public_key_path`）；`[VALUE]` 可省（省则读 stdin） | 明文 → `ENC[...]`（X25519 信封），尾换行剥除一个 |
| `open` | `-k <priv.pem>` 或 `-c <config.yaml>`；`[VALUE]` 可省 | 密文 → 明文 |

---

## 4. 配置

```yaml
secrets:
  private_key_path: keys/secrets-private.pem   # 相对 config 目录；**只放部署机**
  public_key_path:  keys/secrets-public.pem    # 可选，`oj secret seal` 缺省取它

db:
  default: "ENC[Ab3…]"          # 整条 DSN 一起封，不必拆出密码字段
redis:
  default: "ENC[Cd9…]"
auth:
  jwt_secret: "ENC[Ef1…]"
smtp:
  default:
    host: smtp.example.com
    pass: "ENC[Gh2…]"           # smtp.*.pass / xoauth2.access_token
blob:
  backends:
    img: { driver: s3, access_key: "ENC[…]", secret_key: "ENC[…]" }
ldap:                            # 不透明段里的字段同样支持
  default:
    url: ldap://dc.example:389
    bind_dn: "cn=admin,dc=example,dc=com"
    bind_pw: "ENC[…]"
```

**私钥三通道**（优先级高→低）：

| 通道 | 适用 | 风险 |
|---|---|---|
| `OJ_SECRET_KEY`（PEM 内联） | **临时排障** | 会落到 `/proc/<pid>/environ`、`docker inspect`、k8s pod spec、CI 的 env 回显 |
| `OJ_SECRET_KEY_FILE`（路径） | 容器/CI 挂载密钥文件 | 低 |
| `secrets.private_key_path` | 物理机/虚机常驻 | 低（相对 config 目录，注意别把 PEM 提交上去） |

**每环境用独立密钥对。** 信封里没有 key-id，同一对密钥下**把 dev 的密文粘进 prod 照样能解**
（反之亦然）。这是刻意不做 key-id/AAD 的代价，靠密钥分离来兑现环境隔离。

---

## 5. 生效范围与实现位置

解密发生在 `src/config.rs::load_from`：**先解析成 `serde_yaml::Value` → 递归把 `ENC[...]`
就地替换 → 再反序列化成 `Config`**。

- 好处一：`ldap` / `plugins` / `kafkas` 是**不透明 Value**（类型层拦不住里面的 `bind_pw`），
  Value 层递归才能全覆盖；将来新增任何配置段自动生效。
- 好处二：**配置 schema 零改动**，`Config` 结构体与各轴装配完全不知道有这回事。
- 兼容性：配置里没有 `ENC[...]` 时**完全不碰密钥路径**（不配私钥照常启动）；解密后回经
  文本再 `from_str`，以保住 YAML 标量→字符串的隐式转换（`vars: {PORT: 3000}` → `"3000"`）。

---

## 6. 失败形态（全部 fail-fast，无静默降级）

| 情形 | 表现 |
|---|---|
| 有 `ENC[...]` 但三通道都无私钥 | `config has ENC[...] sealed values but no decryption key: …` 退出 |
| 私钥与加密公钥不是一对 | `aes-gcm open failed (sealed value tampered?)`（临时密钥由收件方公钥派生，错钥则派生不出同一会话密钥） |
| 密文是已移除的 v1（RSA） | `sealed value is v1 (RSA) — v1 信封已移除；请用 oj secret seal 以 X25519 重新加密` |
| 密文被改一个 bit / 截断 | `aes-gcm open failed (sealed value tampered?)` / `truncated` |
| 密文版本未知（非 0x02） | `sealed value version N unsupported … 用 oj secret seal 重新加密` |
| 密封值落在 mapping **键**位 | `sealed value used as a mapping key …` |
| 密文塞进数值字段（`server.port`） | `invalid type: string`（解密结果是字符串，见 §7） |
| 明文恰好以 `ENC[` 开头、`]` 结尾 | 当密文硬失败，报错里提示换写法 |

**绝不**存在「解不开就当明文用」的分支——那会连上一个名叫 `ENC[…]` 的密码。

---

## 7. 限制

1. **只能加密字符串值**。解密结果必然是字符串，`port` / `limit` 这类数值字段收到密文会
   `invalid type: string`。
2. **平台不判断「该不该加密」**，只判断「是不是密文」——把非敏感值加密也不会报错。
3. **轮换密钥对必须重封全部密文**（旧私钥解不了新密文，反之亦然）。单个密码换值只需
   `seal` 出新密文粘回去。
4. 明文仍会以字符串形式存在于内存，并经 `plugin_cfg()` 序列化 JSON 透传给 cdylib 插件
   ——插件要用凭据连接，这是必然代价，不属于新增暴露面。

---

## 8. 日志脱敏（配套修复）

DSN / URL 出现在错误信息与 warn 里时，**凭据段打 `***`，host/库名保留**：

```
unknown db scheme in dsn 'oracle://***@db.internal:1521/app'
redis://***@127.0.0.1:6379/1
```

覆盖点：宿主 `src/bridge/db_backend.rs`（两条 DSN 报错）、`oj/src/app.rs`（redis warn），
以及插件 `oj-kv-redis`（三条连接错误）、`oj-bus-rabbitmq`（连接错误）——插件只依赖
`oj-plugin-ffi`，故本地同口径实现了 `redact_url`。

**为什么必须一起修**：`server::logging` 把终端输出完整镜像落 `logs/`，加密了却在日志里
明文打一遍等于白做。

---

## 9. 迁移步骤（存量项目）

1. `oj secret keygen --out-dir keys` + `.gitignore` 加私钥。
2. 逐个把明文密码换成密文：`echo -n '<原值>' | oj secret seal -k keys/secrets-public.pem`。
   整条 DSN 直接封，不要拆字段。
3. config 加 `secrets.private_key_path`（**只写路径，不写 PEM 内容**）。
4. 验证：`oj secret open -c config.yaml '<密文>'` 出原值；`oj serve` 起得来。
5. 部署：公钥随仓库/CI 走；私钥分发到各部署机（`OJ_SECRET_KEY_FILE` 或
   `private_key_path`），权限 600。
6. **git 历史里的旧明文**不会因为这次迁移消失——若已提交过，按泄漏处理（改密码 +
   必要时清历史）。

---

## 10. 排障

| 症状 | 原因 / 处置 |
|---|---|
| 启动报 `no decryption key` | 部署机没给私钥（三通道见 §4）；`oj secret open -c config.yaml` 可复现 |
| `aes-gcm open failed (sealed value tampered?)` | 私钥与公钥不是一对（换机器只拷了 config） |
| `oj secret seal` 出来的值解不开 | 用了别的公钥加密；确认 `-k` 指向的 PEM 与私钥同源 |
| 密文里解出来多了个 `\r` 或换行 | 用 `--value`/positional 传了带换行的值；改走 stdin（尾换行会剥一个） |
| 换了密钥对老密文全解不开 | 预期行为——用新公钥重封全部密文 |
| 想确认某个密文是哪个环境的 | 信封无 key-id，只能靠密钥分离；每环境独立密钥对 |

## 11. FAQ：能否与 `oj-cert` 的证书密钥共用一对？

**不能。** 两套密钥**算法与格式都不同**——密封密钥是 `BEGIN OJ X25519 …`（Curve25519，
用于 ECDH 派生会话密钥），证书密钥是 RSA（`BEGIN PRIVATE KEY`，用于 RS256 签名），
`secret::parse_private_key` 现在只认 X25519，连 `oj-cert` 的 RSA 私钥都读不进来；何况
**用途与部署拓扑相反**，共用会直接削弱证书门禁。

| | 证书密钥（`tools/oj-cert`） | 密封密钥（`oj secret keygen`） |
|---|---|---|
| 算法 / 格式 | RSA（`BEGIN PRIVATE KEY`，RS256 签名） | X25519（`BEGIN OJ X25519 PRIVATE KEY`，ECDH 派生会话密钥） |
| 私钥用途 | **签名**（签发 cert.jws） | **ECDH 解密**（解开 config 密文） |
| 私钥该在哪 | **签发方**（构建机 / 离线冷存；签发才拿出来） | **每台部署机**（否则起不来服务） |
| 公钥去哪 | 部署机（`server.public_key_path`，ring 验签） | 开发者 / CI（加密用，可进仓库） |
| 轮换触发 | 证书续期（renew 重签） | 重封全部密文 |

四条理由：

1. **拓扑相反（决定性）**：共用 = 把「签发合法证书」的能力下发到每台部署机。拿到它的人
   能签一张永不过期的证书，**直接绕过证书门禁**（过期 / 宽限 / 身份校验全部失效）。
   部署机本该只有 `server.public_key_path` 那个**公钥**。
2. **一钥一用途**：NIST SP 800-57 Pt.1 §5.2——同一密钥不得用于多个用途，签名与密钥协商
   尤其不得共用（历史上 Bleichenbacher / CVE-2006-4339 正是同一 RSA 密钥既加密又签名时，
   解密 oracle 辅助伪造签名）。两套算法本就不同，更不该混用。
3. **轮换互相绑架**：共用后换证书密钥就必须重封所有密码，反之亦然——两个本该独立的
   运维流程被绑死。
4. **备份策略冲突**：证书私钥应冷存（少碰），密封私钥必须热备（丢了就起不来服务）。

**反方向也不成立**：用证书公钥当密封公钥更不行——密文只有签发方能解，部署机（只有公钥）
根本解不开。

**正确姿势**：部署机上放两样东西——证书的**公钥**（本来就有）+ 密封的**私钥**；证书私钥
永不离开签发机。文件名也刻意分开（`private.pem`/`public.pem`/`cert.jws` vs
`secrets-private.pem`/`secrets-public.pem`），避免误拿。

## 相关文档

- `docs/devkit/api-manual.md` §10「secrets —— 凭据密封」
- `docs/devkit/scenarios.md` 场景 18「config 里的密码不落明文」
- `docs/devkit/SKILL.md`「常见陷阱速查」
