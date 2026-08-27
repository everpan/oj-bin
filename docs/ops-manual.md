# oj server 运维手册

面向部署、发布、排障。功能面见 `docs/user-manual.md`，实现面见 `docs/dev-manual.md`。

## 1. 构建与发布

```bash
cargo build --release
ls -lh target/release/oj          # 独立二进制，无运行时依赖（deno_core 内嵌）
```

发布物 = `target/release/oj` + 项目目录（`dist/` + `config.yaml` + `seed.sql` + `node_modules/`）。

发布流程：
1. `cargo build --release`（确认 debug/release 双绿）。
2. `oj build -d src -o dist`（无参 = 全部模块）——生成各模块版本目录
   `dist/<module>-<version>/`（产物保留 src 目录结构与原名，如 `account/api.js`，
   默认 minify 成单行）、锁文件 `dist/manifests.yaml` 与确定性发布包
   `dist/<module>-<version>.tgz`（同输入重复打包字节一致，可校验完整性）。
   排障需要可读产物时加 `--no-minify` 重建。
3. 打包 `oj` 二进制 + `dist/` + `config.yaml` + `seed.sql`（可选）+ vendored
   `node_modules/`（裸 specifier 运行时解析依赖它，**不打进 tgz**）。
4. 目标机解包，`./oj server -c config.yaml -d dist`（dist 含 `manifests.yaml` → 自动 release 跑 `.js`）。

## 2. 运行

```bash
./oj server -c config.yaml -d dist            # release
./oj server -c config.yaml -d src              # dev（无 manifests.yaml 自动判定；跑 .ts，改文件即生效）
```

启动时打印模块清单 + 路由表，可据此核对发布是否完整。

## 3. 配置管理

`config.yaml` 全字段可省，均有默认。生产要点：

- **端口**：代码默认 `778`，但属 macOS/Linux 特权端口（<1024），需 root；**生产用 ≥1024**（如 9778）。
- **前缀** `server.base`：API 基础路由前缀（默认 `/v1/api`），随配置走版本管理；
  临时调试可用 `-b` 覆盖。空前缀（空串/纯斜杠）启动即报错。
- **超时** `server.timeout`：单请求熔断阈值（`"30s"` 等）。设太大会放大死循环占用；设太小误杀慢查询。
- **并发** `server.pool_size`：JS 执行线程数，等于并行请求上限。过高吃内存，过低排队。
- **静态站点** `server.root`：静态文件根（相对 config 目录）。API 未命中的 GET/HEAD 落此目录
  （目录 → `index.html`）；目录缺失启动即报错。前置站点产物（如 oj build 的 dist）放独立目录。
- **DB** `db.<name> = "<DSN>"`：相对 config **所在目录**（`config_dir_of` 保证非空）。
  v0.2 多库混用：`sqlite://`（缺文件自动建空库）/`mysql://`/`postgres://`（透传，连不上启动
  fail-fast）。`sqlite::memory:` 仅测试用，重启即丢。**seed.sql 只对 sqlite 的 default 重放**，
  mysql/pg 的建库/迁移归运维。
- **Redis** `redis.default`：v0.2 **配置即真连**（启动 fail-fast，连不上直接报错退出，不会静默退回
  内存）。配置后 `kv`/`redis` 全局与 auth 会话共享同一 Redis，**多实例部署即共享会话与 KV**
  （水平扩展的前提）。不配置 = 进程内存 KV，多实例不共享——把跨实例一致状态放这里前先确认已配 Redis。
- **ES** `es.endpoint`：块存在即启用 `es.search/index/del`（薄直通，index/id 白名单限
  `[a-zA-Z0-9_-]+`）。endpoint 尾斜杠自动剪除。生产接入内网 ES 前先核对 `no_proxy`/网络可达性
  （EsClient 用 no_proxy 独立连接，不随环境代理）。
- **租户** `tenant.enable / tenant.header_key`：启用后所有 `{base}` 请求必须带该 header
  （缺失/空 → 400），值注入 `http.tenantId` 供 handler 做数据隔离（**框架不自动改写 SQL**，
  行级过滤归业务）。
- **鉴权** `auth.*`：块存在即启用。`jwt_secret` 生产必改且入库保管（泄露 = 任意人可签
  合法 token）；`access_token_duration` 短（分钟级）+ `refresh_token_duration` 长（天级）
  是常规组合——access 无服务端吊销，只能等它过期；refresh 可经 logout 主动失效。
- **seed.sql**：项目根存在则启动时对 `default` 库重放。语句按 `;` 切分 → **seed 内不得有分号
  字面量**；用 `INSERT OR IGNORE` 保证可重复执行。
- **blob** `blob.*`：块存在即启用（`blob.put/get/del/url` + `{base}/blob/{key}` 下载路由）。
  - `driver: local`：`root` 相对 config **所在目录**绝对化（缺目录自动建）。进程需写权限
    （上传失败打 `blob put:` 错误）。下载路由公开免鉴权——**不要把需鉴权的对象塞进去**。
  - `driver: s3`：`bucket`/`region` **必填**（缺失启动 fail-fast）；`endpoint`/`access_key`/
    `secret_key` 可选。MinIO/自建 S3 用 `path_style: true`（默认 virtual-hosted 风格，
    自建对象存储通常不支持）。`blob.url()` 走 GET presign 15min；下载路由 302 跳转。
    生产密钥经环境/密钥管理注入，勿硬编码进 config.yaml。
- **证书校验** `server.public_key_path` / `server.certificate_path` / `server.grace_days`：两者都配齐即
  启用基于非对称加密（RSA-2048 + RS256 JWS）的**证书驱动 GET 限制**（详见 `dev-manual.md` §5.1）：
  - 有效期内 / 未配置 → 正常服务。
  - 过期进入宽限期（默认 30 天，可配 `grace_days`）→ 所有 **GET** 返回 `403`
    （JSON `{"error":"certificate expired",...}`），其余方法正常；服务不中断，运维替换证书即恢复。
  - 宽限期结束后再启动 → 记 `ERROR` 后进程退出（exit 1），不提供服务。
  - 证书 / 公钥文件被覆盖即**热加载**（notify 事件驱动，不轮询 mtime），原子更新证书状态；
    重载失败保留旧状态并记 `warn`。`GET {base}/health` 实时返回 `certificate_status` 供监控。

## 4. 热重载语义

- **dev 模式**：`api.ts` 及其依赖按 mtime 缓存；改文件后下次请求用新代码（mtime 版本化 specifier）。
- **release 模式**：跑编译好的 `.js`，同样按 mtime 失效（dist 更新即生效，无需重启）——
  但**版本目录布局下换版本需重启**：`dist/manifests.yaml` 仅启动时读取，运行中改锁指向
  新版本目录不会生效。同版本重建（清场重写同目录）靠 mtime 失效即时生效。
- **证书 / 公钥文件**：`public_key_path` / `certificate_path` 指向的文件被覆盖即**热加载**
  （notify 事件驱动，不轮询 mtime），原子更新证书状态（valid ↔ grace ↔ expired）；重载失败
  保留旧状态并记 `warn`。这与 `config.yaml` 不同——证书轮换**无需重启**。
- **不触发热重载**：`config.yaml`（重启生效）、`seed.sql`（仅启动重放）、`manifest.yaml` 新增/删除
  模块（重启生效）、`node_modules` 新增包（重启生效，已加载包缓存于进程）。

## 5. 超时与资源

- 超时 handler → 对应 JsRuntime 被 `terminate_execution` 强杀并**丢弃不回池**，HTTP 408。
  server 不崩，后续请求正常（`../oj/tests/e2e.rs` 的 `uc12` 验证了这一点）。
- `RuntimePool` 最大空闲 16；池在负载后自动收缩。被杀的 runtime 会即时从池移除。

## 6. 日志

`tracing-subscriber` 输出：启动横幅（模块/路由表）、请求日志（方法/路径/状态/耗时）、
`log.*`（handler 内 `log.debug/info/warn/error(msg, ...kv)` 结构化输出）。另有
`warn: redis '{name}' ({url}) ignored`——`redis` 段配了 `default` 之外的键（仅 `default` 被使用）。
生产建议用 `RUST_LOG` 控制级别：

```bash
RUST_LOG=oj=info ./oj server -c config.yaml -d dist
```

## 7. 排障表

| 症状 | 原因 | 处置 |
|---|---|---|
| 启动即报「missing manifest.yaml」 | 某首层子目录缺 `manifest.yaml`，或残留空目录 | 补齐；删除空目录（空目录不参与 git，但 `read_dir` 会扫到） |
| 启动报「manifest name mismatch」 | `manifest.yaml` 的 `name` ≠ 父目录名 | 对齐 |
| 启动报 `manifests.yaml … run oj build first` | release 下锁文件缺失/损坏，或指向不存在的版本目录 | 跑 `oj build <module>`；锁被手工改坏时按报错修 |
| 启动报「version dir collision」 | 两个 (module, version) 组合拼出同一目录名（如 `a`/`1-x` 与 `a-1`/`x`） | 改 version 命名避开 |
| 404 | 路由无对应 `api.ts/js`，或目录穿越/非法段 | 核对路径与 `-b` 前缀；release 先确认模块在锁内 |
| 启动报 `server.root …` | 静态根目录不存在（相对 config 目录解析） | 建目录或改路径；不配 `root` 即关闭静态服务 |
| 静态文件 404 | 文件不存在 / 目录缺 `index.html` / 非 GET/HEAD / 无 SPA 回退（v0.1） | 核对文件；SPA 场景先经前置反代补写回退 |
| 405 `method 'del' not exported` | `DELETE` 请求但 handler 没导出 `del`（不是 `delete`） | 改导出名 |
| 500 信封含 `api.ts` 字样 | TS 编译/解析错误 | 看 msg 定位行号 |
| 408 | handler 死循环/超时 | 查死循环，或调大 `server.timeout` |
| 未知 DSN scheme 启动失败 | 仅支持 sqlite/mysql/postgres | 检查 `db:` 各 DSN 前缀 |
| mysql/pg 连接失败启动即退 | fail-fast 语义（连接串错/库未建） | 核对 DSN 与目标库可达性 |
| 启动 warn `seed.sql skipped` | default 库非 sqlite，seed 不重放 | mysql/pg 建库归运维 |
| `redis` 数据不跨实例 | `redis.default` 未配置 → 进程内存 KV | 配真 Redis（配置即真连，多实例共享会话/KV） |
| 启动报 `redis 'default': …` 连接失败 | Redis 不可达/未起，fail-fast 直接退出（不静默退回内存） | 起 Redis 或核对 URL/网络；**不想依赖 Redis 就把 `redis:` 段注释掉** |
| 非 `default` 的 redis 键无效 | 仅 `redis.default` 被使用，其余 warn 忽略 | 核对命名；只配 `default` |
| `bus.publish` 收不到广播 | bus 是**进程内**广播，跨实例不互通（发布与 WS 订阅须在同一实例） | 确认发布与订阅同实例；多实例跨进程广播暂不支持 |
| `GET {base}/…/ws` 404 | release 下 WS.ts 未重新 build 进 dist，或 URL 含版本段 | 先 `oj build`；release URL 为 `…/news-0.1.0/ws`（v0.2 已知限制） |
| 400 `missing tenant header: X-TENANT-ID` | `tenant.enable: true` 且请求未带（或值为空）该 header | 客户端补 header，或关掉 `tenant.enable` |
| 401 `missing or invalid bearer token` | `auth:` 启用且路径不在 `anonymous_paths`，请求未带/篡改/过期 access token | 走 `/auth/login` 换新 token；长期化用 refresh 轮换 |
| 401 `invalid or expired refresh token` | refresh token 已被轮换/logout 或 session 过期 | 重新 login；refresh 一次一用是轮换语义，不是故障 |
| 启动报 `auth.jwt_secret must not be empty` | auth 块配置了但 secret 为空串（fail-fast，不静默裸奔） | 填 secret |
| 500 信封 `transaction already active` | 同一请求内嵌套 `db.tx`（每请求仅一个活跃事务） | 合并为一个 `db.tx` 回调，或先完结再开 |
| 日志 `open transaction on db '…' rolled back at request end` | handler 未等待 `db.tx` 结束（漏 await / 中途 throw）即返回 | 修 handler：`await db.tx(...)`；数据已按未提交丢弃 |
| 端口占用 | `778` 需 root | 换 ≥1024 端口 |
| 改 `api.ts` 不生效 | release 下 `dist/` 未更新 / 已加载包缓存 | 确认 dist 同步；必要时重启 |
| 启动报 `blob.driver must be local\|s3` | config `blob.driver` 值非法 | 改成 local 或 s3 |
| 启动报 `blob s3: bucket required` / `region required` | s3 驱动缺 `bucket`/`region`（fail-fast） | 补全；endpoint/密钥可省 |
| 上传/下载 500 `blob put:/get:` | local 根不可写 / s3 连接失败 / 对象不存在 | 查根目录权限与磁盘；s3 核对 DSN、网络、MinIO 是否 path_style |
| `GET {base}/blob/x` 404 | 对象不存在，或 key 含非法段（`.`/`..`/`\`/NUL/空，含编码走私 `%2e%2e`） | 核对 key 与编码；下载路由免鉴权，公开对象才放这里 |
| `blob not configured` 报错 | JS 调 `blob.*` 但 config 无 `blob:` 段 | 加配置；该 op 仅在启用时可用 |
| `es not configured` 报错 | JS 调 `es.*` 但 config 无 `es:` 段 | 加 `es.endpoint`；或确认业务不该用 ES |
| `es search/index/del: invalid index` | index/id 含白名单（`[a-zA-Z0-9_-]+`）外字符 | 校验入参；index/id 不可含 `/`、`.` |
| `es …: HTTP 4xx/5xx: …` | ES 端错误（索引缺失/DSL 错/ES 未起），直通返回体 | 看返回体排障；`es.index` 自带 refresh=true，写完即可查 |
| 启动报 `certificate expired` / `certificate has expired and grace period elapsed` | 证书已过期且宽限期结束（`exp` + `grace_days` 仍早于现在） | 重签续期：构建 `cargo build -p oj-cert --release`（工具在 `tools/oj-cert`，不随发行包）后 `oj-cert renew -k private.pem` 使 `exp` 晚于现在，替换证书文件后重启（运行中替换则热重载即时生效）；调大 `grace_days` 仅延长宽限、不改 `exp` |
| GET 全部 403 `certificate expired` | 运行中证书被热加载切到 grace / expired（或启动即处该状态） | 替换证书文件（热加载即时生效）；查 `GET {base}/health` 的 `certificate_status` |
| 启动报 `invalid public key` / `signature verification failed` | 公钥 PEM 非法，或 JWS 签名与公钥不匹配 | 核对密钥对一致、签名算法为 RS256；用同一私钥重签 JWS |
| 启动报 `invalid JWS format` | `certificate.jws` 不是三段 `Base64URL(Header).Payload.Signature` | 按 `Header.Payload.Signature` 重新生成 JWS |
| 启动报 `certificate not configured` | 仅配了 `public_key_path` 或 `certificate_path` 之一 | 两个路径都配齐才启用校验（都不配 = 不校验） |

## 8. 回滚与恢复

- 配置/代码均有版本；二进制与 `dist/` 打包发布，回滚 = 换回上一版打包产物。
- **多版本共存回滚**：`dist/` 内旧版本目录不被构建清除（仅锁内当前版本的同名目录清场重建），
  回滚单模块 = 把 `dist/manifests.yaml` 该模块指回旧版本 + 重启 server（锁仅启动时读）。
- sqlite 数据文件随 `db.default` 路径落盘；升级前备份 `*.sqlite`。
- 外部依赖可选且默认关闭：不配 `redis:`/`es:` 段即纯本地（sqlite + 内存 KV），故障面最小——
  主要是「二进制 + dist 不一致」→ 保持二者同版本发布。配了 Redis/ES 则它们的可用性进入启动
  契约（fail-fast），发布/巡检时先确认目标实例可达。
