# oj serve 运维手册

**给谁读**：负责部署、发布、排障的人——拿到一台服务器之后照这本走。功能怎么配见
`docs/user-manual.md`，实现细节见 `docs/dev-guide.md`，模块数据层与迁移运维见
`docs/migration.md`。

## 1. 构建与发布

```bash
cargo xtask build          # release 构建，产物归置 bin/oj + bin/plugins/<triple>/（独立二进制，deno_core 内嵌）
```

发布物 = `bin/oj` + `bin/plugins/<triple>/` + 项目目录（`dist/` + `config.yaml` + `seed.sql` + `node_modules/`）。

发布流程：
1. `cargo xtask build`——构建 `bin/oj` 和全部第一方插件（本仓库只支持 release 构建，
   debug 不在支持范围）。
2. `oj build -d src -o dist`（无参 = 全部模块）——生成各模块版本目录
   `dist/<module>-<version>/`（产物保留 src 目录结构与原名，如 `account/api.js`，
   默认 minify 成单行）、锁文件 `dist/manifests.yaml` 与确定性发布包
   `dist/<module>-<version>.tgz`（同输入重复打包字节一致，可校验完整性）。
   排障需要可读产物时加 `--no-minify` 重建。CI 可在构建前跑 `oj build --check`
   （只跑结构检查 S001–S008 不落盘，违规 exit 1）。
3. `oj migrate -c config.yaml -d dist`——应用各模块 `migrations/*.sql` 并按 `schema.yaml`
   收敛（release 默认 `migrate_on_start: verify` 门禁要求账本不落后，**先迁移后启动**）；
   存量库接入用 `--baseline`。发布前可跑 `oj schema diff` 做声明 vs 实库对账（漂移 exit 1）。
   **多库部署**：上面两条默认只作用 `db.default`，非 default 的命名库须加 `--db <profile>`
   逐库各跑一遍（未声明的库名 fail-fast；见 `migration.md` §3.8）。
4. 打包 `bin/oj` + `bin/plugins/<triple>/` + `dist/` + `config.yaml` + `seed.sql`（可选）+
   vendored `node_modules/`（裸 specifier 运行时解析依赖它，**不打进 tgz**）。
5. 目标机解包，`./oj serve -c config.yaml --api-path dist`（dist 含 `manifests.yaml` → 自动 release 跑 `.js`）。
   插件发现走 `<exe>/plugins/<triple>/`，保持 `bin/` 的相对布局拷过去即可，无需额外配置。

### 1.1 npm 分发（`@oj-bin/*`）

除 Release 归档外，另有一条 npm 渠道：`npm i @oj-bin/oj` 由 postinstall 把二进制落到
用户项目 `./bin/`（`bin/oj`、`bin/plugins/<triple>/`、`bin/devkit/`）。主包同时声明了 `bin`
（`npm/oj/bin.js` 启动器，定位平台子包并 exec 其 `oj` 二进制），因此也支持一次性运行，
无需项目内安装：

```bash
pnpm dlx @oj-bin/oj --version
npx  @oj-bin/oj serve -c config.yaml --api-path src
```

`bin.js` 与 `postinstall.js` 共用同一份 `platform-arch → triple` 反向表；`--omit=optional`
下子包不可达，启动器回退到与 postinstall 相同的「平台子包未找到」提示并退出 1。

**模板位置**（仅声明，不含发布逻辑）：

| 路径 | 内容 |
|---|---|
| `npm/oj/` | 主包 `@oj-bin/oj` 模板（`__VERSION__` 占位 + 3 个平台 optionalDependencies）+ `postinstall.js` + `bin.js`（dlx/npx 启动器）+ `test/{postinstall,bin}.test.js` |
| `npm/platform/` | 平台子包 `@oj-bin/oj-<triple>` 模板（`__TRIPLE__`/`__OS__`/`__CPU__`；**禁止加 `exports` 字段**，见包内注释） |
| `npm/README.md` | 两类包共用 README（发布时拷入包根） |

发布逻辑的单一真相来源是 `scripts/npm-publish.sh`——CI（`release.yml` 的 `publish-npm`
job）与本地手动发布走同一脚本。首选走 CI：打 tag 推送即自动发布，无需本地操作。

**发布鉴权**：npmjs 账号启用 2FA 后，CI 走 **Trusted Publishing（OIDC）**——`publish-npm`
job 加 `permissions: id-token: write`，npm CLI 自己换短时凭证，仓库里没有长期 token
（`NPM_TOKEN` 已移除）。npmjs 侧需给 4 个包各配一遍 trusted publisher（`everpan/oj-bin` /
`release.yml`）：网页点击路径、`npm trust` 批量命令、验证与排错见
`docs/superpowers/specs/2026-09-11-npm-publish-design.md` §5.1，约束与副作用见 §7。

**手动发布 / 补发**：脚本只认 GitHub Actions 的 OIDC 环境（本地没有，也不接受
`NODE_AUTH_TOKEN` 兜底），故补发**一律走 CI**——`workflow_dispatch`（同 tag、`draft=false`）
或 Re-run failed jobs 只重跑 `publish-npm` job（幂等，已发布的包自动 skip）。

本地只能演练（不发真包，需 npm ≥ 11.5.1）：

```bash
# dist/ 需备齐 oj-v<ver>-<triple>.tar.gz / .zip（CI package job 的 dist-* artifact，
# 或 gh release download；本地 scripts/deploy.sh 仅产本机 triple）
ACTIONS_ID_TOKEN_REQUEST_URL=x ACTIONS_ID_TOKEN_REQUEST_TOKEN=y \
  DRY_RUN=1 bash scripts/npm-publish.sh v0.1.13   # 只装配+断言，不发布
```

脚本内建门禁与顺序，无需人工操心：

1. **版本一致性**——tag（去 `v`）必须等于 `oj/Cargo.toml` 的 version。
2. **triple 校验**——未知 triple、同 `(os,cpu)` 撞车直接报错。
3. **先子包后主包**——平台包逐个 publish，任一失败即退出，绝不发主包；装配时断言包根
   有 `oj[.exe]`、`plugins/<triple>/`、`devkit/api-manual.md`。
4. **发布后置信**——`npm view` 断言 os/cpu，下载 tarball 断言文件清单。

三点注意：

- **npm 包不可撤回**——先 `DRY_RUN=1` 过一遍。这也是 CI 草稿模式（dispatch +
  `draft=true`）跳过 publish 的原因：人工核对 Release 后重新 dispatch 同 tag、`draft=false`
  幂等补发。
- 主包 optionalDependencies 与实际产物 triple 集**必须互为充要**，产物不全会直接报错；
  完整发布通常还是下载 CI 全量 artifact 再本地跑脚本。
- 启用 musl 目标前必须先定 libc 策略——`os`/`cpu` 无法区分 `gnu` 与 `musl`，
  同 `(os,cpu)` 两个 triple 会在第 2 步撞车。

### 1.2 插件目录与清单锁（ABI 12）

插件是**独立 cdylib**，与二进制之间靠 FFI 契约（`ABI_VERSION`，当前 **12**，**严格相等**）。
`cargo xtask build` 归置出的目录形态：

```
bin/oj
bin/plugins/<host-triple>/
    liboj-db-mysql.dylib      # 文件名 = lib<descriptor.name>.dylib（macOS）/.so（Linux）/<name>.dll（Windows，无 lib 前缀）
    liboj-db-postgres.dylib
    …
    plugins.lock.json         # [{"name","abi","fingerprint"}]，xtask 构建末尾写入
```

三条纪律（全部有代码门禁兜底，违反会在启动期被点名）：

1. **`bin/oj` 与 `bin/plugins/<triple>/` 必须来自同一次构建**。ABI 严格相等：拿 ABI 11 的插件
   配 ABI 12 的二进制 → **拒载**（`plugin ABI mismatch: …`），服务起不来。跨版本升级时两者一起换。
2. **整体拷贝，禁止增量覆盖**。目录里出现锁未登记的库文件 → **拒载**并点名
   （`plugin file not in plugins.lock.json: …`，典型成因：只拷了新插件、旧的没删）。
3. **`cargo xtask plugin <name>` 只拷单个库、不刷新锁**——它适合开发期快速替换，**不要**用它
   产出发布物；发布/部署一律 `cargo xtask build` 或直接整体拷贝 `bin/`。

锁与实物不一致的四种情形（**行为列为准**：锁外文件与 ABI 不等是拒载，另两种只告警）：

| 情形 | 行为 | 文案（stderr，逐字前缀） |
|---|---|---|
| 目录里多了锁未登记的库 | **拒载**（点名文件） | `plugin file not in plugins.lock.json: <path> …` |
| 目录里有库但 `abi` 与宿主不等 | **拒载** | `plugin ABI mismatch: plugin=<p> host=<h> …` |
| 锁登记了、目录里没有（少拷） | 告警 | `[oj-plugin] plugins.lock.json lists N plugin(s) missing from this directory: …` |
| 加载出的 `abi`/`fingerprint` 与锁不符（混入别的构建） | 告警 | `[oj-plugin] '<name>': loaded abi/fingerprint differ from plugins.lock.json …` |

`fingerprint`（构建机 rustc/特性指纹）不一致本身**只告警**（`[oj-plugin] fingerprint mismatch: …`）
——它不改变 ABI 形状，但意味着这对二进制/插件不是在同一个工具链下产出的，出问题时不作支持基线。

## 2. 运行

```bash
./oj serve -c config.yaml --api-path dist            # release
./oj serve -c config.yaml --api-path src              # dev（无 manifests.yaml 自动判定；跑 .ts，改文件即生效）
```

启动时把模块清单 + 路由表写入日志，可据此核对发布是否完整。

```bash
tail -f logs/server-*.log     # 默认：日志只落盘，终端静默
./oj serve -c config.yaml --api-path dist --console-log   # 终端也输出
```

后台运行（无 systemd/supervisor 时的轻量守护）：

```bash
./oj serve -c config.yaml --api-path dist --daemon
# → oj serve daemonized (pid 12345)；父进程即退，子进程脱离终端继续跑
kill 12345                       # 停机：SIGTERM 走优雅停机（排空在途请求与任务）
```

`--daemon` 下 stdio 重定向到空设备，启动失败的真因看 `logs/server-*.log`
（unix 走 setsid 脱离控制终端；Windows 走 DETACHED_PROCESS + CREATE_NEW_PROCESS_GROUP）。

日志级别另由 `RUST_LOG` 环境变量控制（如 `RUST_LOG=oj=debug`），配置里不能配。
落盘基于 fd 重定向，**仅 unix**：Windows 上不写日志文件，终端输出强制保留并告警。
**例外**：启动失败的最终退出原因总是直写终端（console 关闭也不例外），便于立即调整。

### 2.1 `oj exec`（v0.1.29）：直接跑脚本

```bash
./oj exec scripts/fix-roles.ts -c config.yaml -- --dry-run   # -- 后 argv 注入脚本 args
./oj exec scripts/fix-roles.ts -c config.yaml --log-file fix.jsonl
./oj exec -e 'console.log(await db.query("select 1", []))' -c config.yaml   # 内联代码（v0.1.50）
./oj exec --repl -c config.yaml                             # 交互式 REPL（v0.1.50；变量不跨行持久，需挂 globalThis）
```

生产排障开 SQL 追踪（v0.1.51）：config 顶层 `db_trace.enabled: true`（release 默认关），
每条 SQL 打 `target="oj::sql"` 结构化日志；`db_trace.redact_params` 默认脱敏（只记参数
个数），设 false 才记明文（仅服务端日志）。`_sql` 信封字段仅在追踪开启时附加。

一次性数据修复/对账/批处理用。**迁移默认 off 是与 server dev 相反的缺省**——
server dev 不写 `migrate_on_start` 时缺省 `auto`（启动即应用迁移），`oj exec` 同样
不写时**全跳过**（apply/verify/reconcile 都不做），仅 config 显式写
`migrate_on_start: auto|verify` 才执行。拿 server 直觉套 exec 之前先核对这个开关。
其他差异：无证书门禁；无请求超时/KillSwitch（同步死循环 Ctrl-C 兜底，**不适合
生产常驻**——常驻任务放 `src/tasks/` 任务池）；`sql_guard: "deny"` 库上脚本须
`db.asSystem()`；stdout 只有脚本输出（装配日志走 stderr），可安全管道/重定向。
专题手册：`docs/exec-integration.md`。

## 3. 配置管理

`config.yaml` 全字段可省，均有默认。生产要点：

- **端口**：代码默认 `9778`（≥1024，无需 root）。若改用 <1024 的端口（如 `778`），
  在 macOS/Linux 属特权端口，需 root。
- **前缀** `server.api_prefix`（旧键 `base` 兼容）：API 基础路由前缀（默认 `/v1/api`），随配置走版本管理；
  临时调试可用 `-b` 覆盖。空前缀（空串/纯斜杠）启动即报错。
- **超时** `server.timeout`：单请求熔断阈值（`"30s"` 等）。设太大会放大死循环占用；设太小误杀慢查询。
- **并发** `server.pool_size`：JS 执行线程数，等于并行请求上限。过高吃内存，过低排队。
- **WS** `ws.max_connections`：全局并发连接闸门（默认 1000，0=不限），超限 upgrade 返 503。
  内存与连接数解耦——V8 只驻留在每路由 `ws.workers_per_route` 个 Worker（默认 2），
  高并发长连接按闸门做容量规划即可（每连接仅 ≈2KB 会话态）。
- **静态站点** `server.app_path`（CLI `--app-path` 可覆盖）：静态文件根。config 配置相对 config 目录；CLI 显式给出时相对 CWD。API 未命中的 GET/HEAD 落此目录
  （目录 → `index.html`）；目录缺失启动即报错。前置站点产物（如 oj build 的 dist）放独立目录。
  **准入门（三态，无静默默认）**：api（`--api-path`）与静态（`server.app_path` / `--app-path`）至少显式指定其一，否则退出；
  两者皆指定 → 都必须存在，任一缺失退出；仅指定其一 → 只启用对应功能（api 缺席 = 纯静态模式，两者皆未指定 = 退出）。
  静态站点前缀 `server.app_prefix`（默认 `/`）：设为如 `/site` 时仅 `/site/*` 的 GET/HEAD 落静态（前缀剥除后解析），前缀外 404；API 路由永远优先。
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
- **证书校验** `server.public_key_path` / `server.certificate_path` / `server.grace_days`：
  **证书必配且不可绕过**——两个路径缺任一即启动报错退出（exit 1），没有任何 config/CLI
  开关可跳过证书校验；`--cert-path`/`--key-path` 仅覆盖路径、不豁免校验。证书生成/续期
  用 `tools/oj-cert`（`cargo run -p oj-cert -- gen -o config` 首次签发、
  `renew -k config/private.pem` 到期续签，详见 `dev-guide.md` §11.2）。配齐后启用基于
  非对称加密（RSA-2048 + RS256 JWS）的**证书驱动 GET 限制**（详见 `dev-guide.md` §11.2）：
  - 有效期内 → 正常服务。
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
- **`ext_boot.js`（可选）**：**不热重载**，改动后必须重启进程。装配期冻结
  `file://…?v=<mtime>` 并打印 `ext_boot: loaded <绝对路径> (<spec>)`，这行日志是核对
  「跑的是不是改过的那份」的唯一依据（池常驻，运行期新建的 runtime 才会重新读盘，
  故改文件不重启会出现池内新旧混杂）。
- **不触发热重载**：`config.yaml`（重启生效）、`ext_boot.js`（重启生效，见上）、`seed.sql`
  （仅启动重放）、`manifest.yaml` 新增/删除模块（重启生效）、`node_modules` 新增包
  （重启生效，已加载包缓存于进程）。

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
RUST_LOG=oj=info ./oj serve -c config.yaml --api-path dist
```

### 6.1 插件装配期日志（逐字，v0.1.88）

插件只在**启动期**加载一次，装完即冻结——下列行是排查插件问题的第一现场（`stderr`，
不受 `RUST_LOG` 影响）。装配失败时整行以 `oj serve: ` 开头并退出码 1。

| 日志（逐字形态） | 级别 | 含义 |
|---|---|---|
| `[oj-serve] host ABI_VERSION = 12` | info | 宿主 ABI。与插件横幅的 `abi=` 必须相等 |
| `[oj-serve] plugin <name> abi=<n> fp=<fp>[ \| caps: placeholder=<p> id=<i>[ stream][ cancel]]` | info | 每个成功装配的插件一行；`caps` 仅 db 轴插件有（`placeholder=?\|$\|:`、`id=returning\|two_step\|none`）。**这是能力位的权威现场**——排障时先看这行，不要猜 |
| `[oj-plugin] fingerprint mismatch: plugin '<name>' built with [<fp>], host [<host-fp>]` | warn | 二进制与插件不是同一工具链产物（ABI 相同才走到这里）。可运行，但不作支持基线 |
| `[oj-plugin] '<name>': loaded abi/fingerprint differ from plugins.lock.json (lock abi=… fingerprint=…; loaded abi=… fingerprint=…) — bin/ 不是同一次构建的整体拷贝` | warn | 混入了别的构建产物（锁对不上实物的那一半） |
| `[oj-plugin] plugins.lock.json lists N plugin(s) missing from this directory: a, b — 整体拷贝不完整（缺插件的能力会表现为 unsupported）` | warn | 锁登记了但目录里没有——少拷了 |
| `[oj-plugin] panic while loading plugin '<name>' (host fingerprint: <fp>)` | error | 插件 `init` 里 panic（已 `catch_unwind`，不会带崩宿主）；带宿主指纹便于对账 |
| `[oj-serve] unconsumed config sections: ["xxx"] (typo? or plugin not loaded)` | warn | config 里有段落没被任何插件消费——通常是段名拼错，或对应插件没装上 |

## 7. 排障表

| 症状 | 原因 | 处置 |
|---|---|---|
| 启动即报「missing manifest.yaml」 | 某首层子目录缺 `manifest.yaml`，或残留空目录 | 补齐；删除空目录（空目录不参与 git，但 `read_dir` 会扫到） |
| 启动报「manifest name mismatch」 | `manifest.yaml` 的 `name` ≠ 父目录名 | 对齐 |
| 启动报 `manifests.yaml … run oj build first` | release 下锁文件缺失/损坏，或指向不存在的版本目录 | 跑 `oj build <module>`；锁被手工改坏时按报错修 |
| 启动报「version dir collision」 | 两个 (module, version) 组合拼出同一目录名（如 `a`/`1-x` 与 `a-1`/`x`） | 改 version 命名避开 |
| 404 | 路由无对应 `api.ts/js`，或目录穿越/非法段 | 核对路径与 `-b` 前缀；release 先确认模块在锁内 |
| 启动报 `server.app_path …` | 静态根目录不存在（config 配置相对 config 目录解析；CLI `--app-path` 相对 CWD） | 建目录或改路径；不配 `app_path` 且不给 `--app-path` 即关闭静态服务 |
| 启动报 `api path not found: …` | `--api-path` 指定的目录不存在 | 改路径或建目录；两者皆指定时必须都存在（准入门三态） |
| 启动报 `static site dir not found: …` | 指定了静态根但目录不存在 | 建目录或改路径 |
| 启动报 `neither api path … specified` | `--api-path` 与静态站点皆未指定（server 无 src/dist 自动搜索兜底） | 显式指定其一：`--api-path` 或 `server.app_path` / `--app-path` |
| 静态文件 404 | 文件不存在 / 目录缺 `index.html` / 非 GET/HEAD / **未开** `server.app_spa_fallback`（SPA 深链默认不回落，v0.1.20） | 核对文件；SPA 站点显式开 `server.app_spa_fallback: true`（`api_prefix` 下的 404 仍 404）；或在反代层做回落 |
| 405 `method 'del' not exported` | `DELETE` 请求但 handler 没导出 `del`（不是 `delete`） | 改导出名 |
| 500 信封含 `api.ts` 字样 | TS 编译/解析错误 | 看 msg 定位行号 |
| 408 | handler 死循环/超时 | 查死循环，或调大 `server.timeout` |
| 启动报 `unknown db scheme in dsn '…' (known: […])` | 没有任何**已装后端**认领该 DSN scheme（v0.1.88 起按认领路由，不再是封闭枚举） | 先看 `known` 里有没有目标 scheme：没有 `mysql://`/`postgres://` = 对应插件没装（`cargo xtask build` 或核对 `OJ_PLUGINS_DIR`）；有的则是 DSN 前缀写错 |
| mysql/pg 连接失败启动即退 | fail-fast 语义（连接串错/库未建） | 核对 DSN 与目标库可达性 |
| 启动 warn `seed skipped (no default db)` | `db:` 段里没有名为 `default` 的库（seed 只对默认库重放） | 加 `default:` 条目，或用 `oj serve --db <name>` 指定 |
| `redis` 数据不跨实例 | `redis.default` 未配置 → 进程内存 KV | 配真 Redis（配置即真连，多实例共享会话/KV） |
| 启动报 `ext_boot: …` 且拒绝启动 | `ext_boot.js` 语法错/导入失败/顶层 await 抛错（启动期预热即 fail-fast） | 看报错定位；改完重启 |
| 启动未打印 `ext_boot: loaded` 但文件已放好 | 文件不在 **config.yaml 同目录**，或名字不是 `ext_boot.js` | 挪到 config 同级；确认文件名 |
| 改了 `ext_boot.js` 没生效 | 不做热重载，装配期已冻结 spec | 重启进程 |
| 运行期偶发 500，msg 含 `ext_boot` | boot 依赖了外部服务（抖动）或文件被删/替换 | boot 改为幂等无外部依赖；恢复文件后重启 |
| `ext_boot.js` 里 `await` 报 SyntaxError | 文件无 import/export，被 CJS 启发式包进非 async 函数 | 加一句 `export {};` |
| `ext_boot.js` 里 `import "ext:core/ops"` 失败 | deno_core 硬拦（`ext:` 只允许从 `ext:`/`node:` 导入），设计如此 | 用已有全局组合；要新 op 属改 bootstrap |
| 启动报 `redis 'default': …` 连接失败 | Redis 不可达/未起，fail-fast 直接退出（不静默退回内存） | 起 Redis 或核对 URL/网络；**不想依赖 Redis 就把 `redis:` 段注释掉** |
| 非 `default` 的 redis 键无效 | 仅 `redis.default` 被使用，其余 warn 忽略 | 核对命名；只配 `default` |
| `bus.publish` 收不到广播 | bus 是**进程内**广播，跨实例不互通（发布与 WS 订阅须在同一实例） | 确认发布与订阅同实例；多实例跨进程广播暂不支持 |
| `GET {base}/…/ws` 404 | release 下 ws.ts 未重新 build 进 dist，或 URL 含版本段；`--module-root` 布局下还要核对是否多/少了一层布局段 | 先 `oj build`；release URL 为 `…/news-0.1.0/ws`（v0.2 已知限制）；module-root 下 `src/wiki/api/ws.ts` → `{base}/wiki/ws`（v0.1.87 起与 api.ts 同口径）。一律以启动打印的 `ws: N mount(s)` 清单为准 |
| 启动报 `ws mount conflict: … both map to GET …` | 两个不同目录的 `ws.ts` 映射到同一 URL（胜者决定执行哪份 JS，故拒启） | 删除或改名其中一个 |
| 400 `missing tenant header: X-TENANT-ID` | `tenant.enable: true` 且请求未带（或值为空）该 header | 客户端补 header，或关掉 `tenant.enable` |
| 401 `missing or invalid bearer token` | `auth:` 启用且路径不在 `anonymous_paths`，请求未带/篡改/过期 access token | 走 `/auth/login` 换新 token；长期化用 refresh 轮换 |
| 401 `invalid or expired refresh token` | refresh token 已被轮换/logout 或 session 过期 | 重新 login；refresh 一次一用是轮换语义，不是故障 |
| 启动报 `auth.jwt_secret must not be empty` | auth 块配置了但 secret 为空串（fail-fast，不静默裸奔） | 填 secret |
| 500 信封 `transaction already active` | 同一请求内嵌套 `db.tx`（每请求仅一个活跃事务） | 合并为一个 `db.tx` 回调，或先完结再开 |
| 日志 `open transaction on db '…' rolled back at request end` | handler 未等待 `db.tx` 结束（漏 await / 中途 throw）即返回 | 修 handler：`await db.tx(...)`；数据已按未提交丢弃 |
| 端口占用 | 配了 <1024 的端口（如 `778`）需 root | 换 ≥1024 端口（默认 `9778`） |
| 改 `api.ts` 不生效 | release 下 `dist/` 未更新 / 已加载包缓存 | 确认 dist 同步；必要时重启 |
| 启动报 `blob.driver must be local\|s3` | config `blob.driver` 值非法 | 改成 local 或 s3 |
| 启动报 `blob s3: bucket required` / `region required` | s3 驱动缺 `bucket`/`region`（fail-fast） | 补全；endpoint/密钥可省 |
| 上传/下载 500 `blob put:/get:` | local 根不可写 / s3 连接失败 / 对象不存在 | 查根目录权限与磁盘；s3 核对 DSN、网络、MinIO 是否 path_style |
| `GET {base}/blob/x` 404 | 对象不存在，或 key 含非法段（`.`/`..`/`\`/NUL/空，含编码走私 `%2e%2e`） | 核对 key 与编码；下载路由免鉴权，公开对象才放这里 |
| `blob not configured` 报错 | JS 调 `blob.*` 但 config 无 `blob:` 段 | 加配置；该 op 仅在启用时可用 |
| `es not configured` 报错 | JS 调 `es.*` 但 config 无 `es:` 段 | 加 `es.endpoint`；或确认业务不该用 ES |
| `es search/index/del: invalid index` | index/id 含白名单（`[a-zA-Z0-9_-]+`）外字符 | 校验入参；index/id 不可含 `/`、`.` |
| `es …: HTTP 4xx/5xx: …` | ES 端错误（索引缺失/DSL 错/ES 未起），直通返回体 | 看返回体排障；`es.index` 自带 refresh=true，写完即可查 |
| 启动报 `certificate expired` / `certificate has expired and grace period elapsed` | 证书已过期且宽限期结束（`exp` + `grace_days` 仍早于现在） | 重签续期：构建 `cargo build -p oj-cert --release`（工具在 `tools/oj-cert`，不随发行包）后 `target/release/oj-cert renew -k private.pem` 使 `exp` 晚于现在，替换证书文件后重启（运行中替换则热重载即时生效）；调大 `grace_days` 仅延长宽限、不改 `exp` |
| GET 全部 403 `certificate expired` | 运行中证书被热加载切到 grace / expired（或启动即处该状态） | 替换证书文件（热加载即时生效）；查 `GET {base}/health` 的 `certificate_status` |
| `oidc not configured` 报错 | JS 调 `oidc.*` 但 config 无 `oidc:` 段（或缺私钥路径未通过启动校验） | 加 `oidc:` 段（`issuer` + `private_key_path`），见 `docs/oidc-integration.md` |
| 启动报 `oidc: issuer must not be empty` / `parse pkcs8 pem` | `oidc:` 段存在但 issuer/私钥路径为空，或私钥不是 PKCS#8 PEM（fail-fast） | 补 issuer；用 `openssl genpkey -algorithm RSA …` 或 `oj-cert gen` 重新生成私钥 |
| `/oidc/login` → 502 `discovery failed` | `oidc.rp.<tenant>.issuer` 不可达 / 无 discovery 端点 /  issuer 写错 | `curl <issuer>/.well-known/openid-configuration` 自查；网络/代理核对 |
| `/oidc/callback` → 401 `invalid or expired state` | state 已消费（一次一用）或超 10 分钟，或 KV 多实例不共享 | 从 `/oidc/login` 重新起流程；多实例部署必须配共享 KV（Redis） |
| `/oidc/callback` → 401 `id_token verification failed` / `claims mismatch` | IdP 轮换密钥（kid 不匹配）、token 被篡改，或 nonce/iss/aud 与快照不符 | 核对 issuer 逐字一致；重登；确认 IdP 签名算法为 RS256 |
| authorize → 400 `redirect_uri not registered` | 跳转地址不在 `oidc.clients.<id>.redirect_uris` 精确白名单 | 白名单为精确串匹配（scheme/host/端口/路径全同），补注册或改地址 |
| OP 登录后 `/idp/authorize` 仍 401 `login required` | `IDP_SESSION` cookie 未带上（跨域丢失/过期/Path 不符）或 OP 会话 KV 过期 | 带 cookie 重试（curl `-b`）；会话 TTL 取 `auth.refresh_token_duration` |
| 启动报 `invalid public key` / `signature verification failed` | 公钥 PEM 非法，或 JWS 签名与公钥不匹配 | 核对密钥对一致、签名算法为 RS256；用同一私钥重签 JWS |
| 启动报 `invalid JWS format` | `certificate.jws` 不是三段 `Base64URL(Header).Payload.Signature` | 按 `Header.Payload.Signature` 重新生成 JWS |
| 启动报 `certificate is mandatory but not configured` | 证书必配（无逃生口）但 `public_key_path`/`certificate_path` 缺任一 | 两个路径都配齐；**没有任何开关可跳过证书校验**——若实例连不上证书，需生成并挂载（见 §3 证书校验） |
| 启动报 `certificate is mandatory … public_key_path is required` / `… certificate_path is required` | 仅配了 `public_key_path` 或 `certificate_path` 之一（缺任一门禁即拒绝，无部分配置状态） | 两个路径都配齐 |
| 启动报 `M004: 模块 … 有 N 个待应用迁移 … verify 模式拒启` | release 默认 `migrate_on_start: verify`，迁移账本 `_oj_migrations（module 列区分模块）` 落后于 dist 内迁移文件 | 先 `oj migrate -c config.yaml -d dist` 再启动；`off` 是逃生门（迁移归运维，不推荐常态） |
| `oj schema diff` 报 D001/D002 退出 1 | 声明（schema.yaml）与实库漂移——手工改库、漏迁移、删除列未走迁移 | 按 diff 报告逐条补迁移或修正声明；发布前跑一次作巡检 |
| `oj migrate` / `test fixture` / `schema diff` 报 `--db "x" not declared in config (db keys: […])` | `--db` 给的 profile 不在 config `db:` 段；工具**不回落** default（防迁错库） | 核对 `db:` 段的键名拼写；多库部署逐 profile 各跑一遍 |
| 启动/build 报结构检查违规（S001–S008） | manifest 合法性（S001）、表归属冲突（S002）、跨模块表未声明 deps（S003）、deps 版本不满足（S004）、tables 与 schema.yaml 不一致（S005）、seed 纪律（S006）、迁移文件序列（S007）、导入别名/deps 门禁（S008） | 按报错「下一步」修复；只查不落盘用 `oj build --check` |
| 启动报 `plugin ABI mismatch: plugin=<p> host=<h> (rebuild plugin against oj-plugin-ffi ABI <h>); plugin fingerprint [<p-fp>] host fingerprint [<h-fp>]; 请重新整体拷贝 bin/，禁止增量覆盖（partial copy 会让新能力表现为 unsupported）` | 二进制与插件跨版本混用（ABI 严格相等，差 1 即拒载）；或插件是同版本但用旧 ABI 头编的 | 用**同一次** `cargo xtask build` 的 `bin/oj` + `bin/plugins/<triple>/` 整体替换；不要只换一侧 |
| 启动报 `plugin file not in plugins.lock.json: <path> …`（后缀为「lock 要求整体拷贝 bin/，禁止增量覆盖；删除该文件或重新 cargo xtask build——注意 cargo xtask plugin <name> 只拷单个库、不刷新锁」） | 增量覆盖残留：目录里多了锁未登记的库（典型：只拷新插件、没删旧文件） | 删该文件，或 `cargo xtask build` 重写锁后整体拷贝；部署不使用 `cargo xtask plugin` |
| 启动报 `plugin identity mismatch: expected '<x>', got '<y>'` | 配置 `plugins:` 清单里指定的名字/版本与库文件的 `descriptor` 不符（文件名须为 `lib<descriptor.name>.so\|dylib\|dll`；清单带 semver 时须逐字相等） | 对齐文件名与清单；或改清单条目 |
| 启动报 `plugin symbol missing: <path> (symbol <符号名>)` | 库文件构建不完整、或用手写符号绕过了 `oj_plugin_entry!`、或与宿主 ABI 不一致导致符号名不同 | 用 `oj_plugin_entry!` 重编插件；`cargo xtask plugin <name> --check` 可预检 ABI/身份/符号 |
| 启动报 `plugins db assembly: plugin db capability error: unknown capability "…"` / 缺组 / 组内多项 | 插件 `describe()` 的能力串不在闭集（8 个）内、两个必选组缺一或重复（typo 最常见，如 `placeholder_colon_`） | 修插件 `describe()` 后重编；宿主**故意拒启**——不让错配活到运行期（契约见 `docs/plugins/plugin-development.md` §15） |
| 启动 warn `[oj-plugin] fingerprint mismatch: plugin '<name>' built with [<p-fp>], host [<h-fp>]` | 二进制与插件来自不同工具链/特性组合（ABI 相同，故只告警） | 生产环境按「整体拷贝 bin/」重拷；留着跑不算故障，但出问题时不作支持基线 |
| 运行期 `OJ_DB_UNSUPPORTED`（`e.data.capability` 点名 API） | 两种完全不同的原因：①方言不是 sqlite/mysql/postgres，宿主不生成 SQL（**预期**）；②插件没声明 `stream`/`cancel` 等能力 | ①改用裸 SQL（`db.query`/`db.exec`/`db.tx` 始终可用）；②按报错点名补插件能力。别自己按 sqlite 语义拼 SQL 绕（`docs/devkit/db-guide.md` §10.5 / §10.7） |
| 配了 `plugins:` 但启动横幅里没有该插件 | 发现路径不对（5 级：`OJ_PLUGINS_DIR` > config `plugins_dir` > 打包布局 > `<exe>/plugins` > `<workspace>/bin/plugins`，再拼 `<host-triple>/`），或文件名不符合 `lib<name>.so` / `lib<name>.dylib` / `<name>.dll`（platform 决定前缀） | 用 `oj info` 的 `plugins` 段核对实际装了什么；对齐目录与文件名；启动横幅 + `host ABI_VERSION` 行是权威现场 |

## 8. 回滚与恢复

- 配置/代码均有版本；二进制与 `dist/` 打包发布，回滚 = 换回上一版打包产物。
- **多版本共存回滚**：`dist/` 内旧版本目录不被构建清除（仅锁内当前版本的同名目录清场重建），
  回滚单模块 = 把 `dist/manifests.yaml` 该模块指回旧版本 + 重启 server（锁仅启动时读）。
- sqlite 数据文件随 `db.default` 路径落盘；升级前备份 `*.sqlite`。
- 外部依赖可选且默认关闭：不配 `redis:`/`es:` 段即纯本地（sqlite + 内存 KV），故障面最小——
  主要是「二进制 + dist 不一致」→ 保持二者同版本发布。配了 Redis/ES 则它们的可用性进入启动
  契约（fail-fast），发布/巡检时先确认目标实例可达。

### 8.1 回滚三元组：`bin/oj` + `bin/plugins/<triple>/` + `dist/`

有插件之后，**「回滚二进制」不再是一个文件的事**。必须整体回到同一次构建的产物：

| 成员 | 为什么不能单独换 |
|---|---|
| `bin/oj` | 单方前进 → 旧插件 ABI 不等 → **全部插件拒载、服务起不来**（`plugin ABI mismatch`）。单方后退 → 新插件同样拒载 |
| `bin/plugins/<triple>/`（含 `plugins.lock.json`） | 只换插件不换锁 → 启动告警「loaded abi/fingerprint differ from plugins.lock.json」（能跑，但部署已不是整体拷贝）；换同名库 → ABI 不等即**拒载**，ABI 相同但 fingerprint 不同则告警；新增/改名的库 → `plugin file not in plugins.lock.json` **拒载** |
| `dist/` | `oj build` 产物与二进制能力面对齐（新 API/新校验在新版本才有）。旧 dist 配新二进制通常可跑但会缺新能力；新 dist 配旧二进制 → 调用新 API 报错或行为分叉 |

**回滚步骤**（假设上一版整包在 `/opt/oj/<ver>/`）：

```bash
systemctl stop oj                                  # 1. 先停（迁移不回滚，见下）
cp -a /opt/oj/<prev>/bin/oj /opt/oj/current/bin/   # 2. 二进制 + 插件目录 + 锁一起换
rm -rf /opt/oj/current/bin/plugins && cp -a /opt/oj/<prev>/bin/plugins /opt/oj/current/bin/
cp -a /opt/oj/<prev>/dist/. /opt/oj/current/dist/  # 3. dist 一起回退（含 manifests.yaml）
./bin/oj info | sed -n '/^## plugins/,/^## generic_axes/p'   # 4. 核对：装了什么、abi 多少、caps 是什么
systemctl start oj                                 # 5. 起服务，看横幅三行
```

**回滚后必须核对三件事**（前两项来自启动横幅，第三项是健康面）：

1. `[oj-serve] host ABI_VERSION = <n>` 与每个插件行的 `abi=<n>` **相等**；
2. 每个 db 插件行都带 `| caps: …`——**没有 `caps` 说明它不是 db 轴插件**（插件行本身缺失才是没装上）；
3. `GET {base}/plugins` 与 `oj info` 的 `plugins` 段一致（`caps` 列与横幅同源）。

**迁移不回滚**：`oj migrate` 是前向的（账本 `_oj_migrations` 只增），回滚二进制不撤销已应用的
迁移。回滚前评估「新版本的迁移是否被旧版本代码接受」——加列/加表通常安全，**改列型/删列不安全**，
必要时用备份恢复数据库而非反向迁移。sqlite 直接换回升级前的 `*.sqlite` 备份最省事。
