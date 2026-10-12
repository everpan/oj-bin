# config 热更新设计（v1）

- 日期：2026-10-12
- 状态：已评审（双专家对抗复核 + 第二轮复核后修订），待实现计划
- 关联：`serve/src/lib.rs`（路由装配）、`oj/src/app.rs`（App 生命周期）、`oj/src/serve_cmd.rs`（启动编排）、`oj/src/tasks.rs`（任务池）

## 1. 背景与目标

`oj serve` 启动后 config.yaml 任何变更都需重启进程。本设计提供运行中热更新：

1. **验证先行**：新配置先走完整验证管线（静态校验 + 影子装配），任何一步失败即拒绝，旧世界继续服务，**绝无半应用状态**。
2. **整份可提交**：任何段的变更都可提交；能热更的即时生效，不能热更的明确列入 `restart_pending` 返回给调用方——**绝无静默不生效**。
3. **零停机切换**：TcpListener 只 bind 一次，切换是路由原子替换；在途请求自然排空。

## 2. 非目标（v1 明确不做）

- 文件 watch 自动触发（`notify` 不引入本特性；运维写盘后显式调 API）。
- SIGHUP 触发（YAGNI）。
- 插件重装 / 后端资源轴（db/redis/blob/es/broker/mail/ldap/auth/smtp）热更——插件二次 init 的重入安全性未经验证（见 §8 复核结论），逐插件审计后另立特性。
- 跨请求配置原子性（切换窗口内新旧并存是交换固有语义，文档红线）。

## 3. 总体架构

```
TcpListener（永生，只 bind 一次）
  └─ 外层 Router（启动期构造一次，静态，只挂一个 fallback，不挂任何中间件）
       └─ fallback 持有 Arc<ReloadState>，按路径分流：
            ├─ 命中某已知 mount prefix 的 {p}/_system/health
            │     → 新位 200；{p}/health 旧别名 → 200 + warn（每命中一条）
            ├─ 命中某已知 mount prefix 的 {p}/_system/config
            │     → 自拦鉴权（见 §4）后走 reload 流程（POST）/ 摘要（GET）
            └─ 其余 → watch::Receiver<Arc<Router>> 取当前内层 Router
                     → 原样 oneshot 转发（不 nest、不剥前缀）
                                    ▲
内层 Router = App.router（业务 + /plugins + 静态挂载 + WS 路由）
热更 = watch::Sender::send(新 Router) + 旧 App 靠 Arc 引用计数自然 drain
```

> **第二轮复核修订**：原设计把 `{p}/_system/*` 作为外层**具体路由**逐 prefix 注册，
> 与「外层启动期构造一次、静态」冲突——重载**新增** api 挂载前缀时该前缀拿不到
> `_system` 端点（违反「绝无静默不生效」）。改为：外层只挂**单一 fallback**，
> `_system`/health 由该 fallback 在 ReloadState 已知的 mount prefix 集合上分流处理，
> 不再以内层路由形式存在。这样既保留「外层静态、只构造一次」，又让 mounts 变化
> （含新增前缀）只需更新 ReloadState 的 prefix 集合即可，且 `_system` 仍在外层、
> 不经内层证书 GET 门禁（满足 §9.5 运维到期可用）。

关键事实（复核确认）：

- `Router` 内部 Arc，克隆廉价；外层 fallback 原样转发即可，`{base}` 之外的路径全部由内层 fallback `handle`（`serve/src/lib.rs:235,440-446`）最长前缀分发正确到达，无双记日志。外层 fallback 本身不挂 `log_requests`/`CorsLayer`，业务中间件只在内层一次性生效，无双重记录/双重 CORS。
- WS upgrade 经 oneshot 机制成立：真 hyper 请求的 `OnUpgrade` 随 `Request` 的 extensions 存活（`axum::extract::ws::WebSocketUpgrade` 从 extensions 取出 `hyper::upgrade::OnUpgrade`），外层 `oneshot` 透传整份 `Request`（含 extensions）后内层 `on_upgrade` 返回 101，`axum::Body` 携带升级后的流由 `axum::serve` 原样驱动。**WS 升级透传是 axum 内部语义，与 `ws.rs` 具体行号无关**；**进程内 dispatch 测试路径构造裸请求测不到 upgrade**，验收必须含真 TCP e2e（§10）。
- 旧 JS 世界回收 = Arc 自然 drain：actor 线程 channel 关且队空自退，isolate 在 actor 线程析构，KillSwitch drop ≤25ms（`serve/src/actor.rs:85-133`、`src/bridge/runtime.rs:364-380`）。**JS RuntimePool 没有 shutdown_and_join，也不要给它加。**
- `{base}` 钉在启动期：热更改 api prefix 后 `_system`/health 仍在旧 base——文档注明（§9）。新增 prefix 经 ReloadState.prefixes 集合自动覆盖（见上修订）。
- `_system` 真实逻辑在外层 fallback，不经过内层 handle 的证书 GET 门禁（`lib.rs:417-434`）——运维期望行为，保留。

## 4. API 面

保留命名空间 `{base}/_system`（base = 每个 api 挂载 prefix；`{base}/_system/*` 由外层 fallback 在 ReloadState 已知 prefix 集合上分流处理，**不是内层路由**）：

| 端点 | 方法 | 语义 | 鉴权 |
|---|---|---|---|
| `{base}/_system/health` | GET | 从 `{base}/health` 迁移；旧路径留别名一版 + warn | 匿名（与现状一致） |
| `{base}/_system/config` | GET | 摘要：`{config_path, sha256, loaded_at, generation, last_reload}`。**不回显内容**（密封值零泄露面） | reload_api 守卫 |
| `{base}/_system/config` | POST | 触发热更：重读磁盘 config.yaml → 验证 → 应用。**无 body**（非空 body 拒收 400） | reload_api 守卫 |

- **`server.reload_api` 缺省 `false`**：不开则 `{base}/_system/config` 走 404 分支（fallback 不认该路径）。热更是基础设施级触发器（可引发 db 重连、全量重装配），与业务路由裸奔不等价，必须显式开启。字段须新增到 `ServerCfg`（`src/config.rs:109` + `Default` 于 `:198`，`#[serde(default)]` 缺省 `false`），见 §7 必改清单 0。
- 守卫：auth 已配置 → 复用 `auth_guard`（AuthGuard/Bearer）；auth 未配置 → **仅回环地址可调**（`ConnectInfo<SocketAddr>` 判定，非 127.0.0.1/::1 → 403）。保住 dev 无鉴权体验，堵死远程无凭据触发。
  - **第二轮复核新增**：回环判定依赖 `ConnectInfo<SocketAddr>`，须由 serve 栈注入。新 `serve_graceful` 必须用 `into_make_service_with_connect_info(SocketAddr)`（或等价 `Connected` 实现）起服务，否则该守卫编译不过/提取失败（当前 `serve_router` 经 `axum::serve` 直起，未注入连接信息）。见 §7.3。
- `{base}/_system` 为保留路径，遮蔽同名业务路由（`/plugins` 同语义）。装配期扫 RouteTable pattern 撞 `{base}/_system/**` → **warn**（先例即静默，warn 一条不算破坏）。
- POST 重读**磁盘文件**而非收 body：密封凭据不出盘、API 内容与磁盘无漂移；`load_with_extra` 的解密管线（`src/config.rs:1379-1417`）原样复用，私钥解析每次生效（`OJ_SECRET_KEY` > `OJ_SECRET_KEY_FILE` > `secrets.private_key_path`，`src/secret.rs:262-289`），失败 fail-fast → 400。

## 5. POST 热更流程

全程 `tokio::sync::Mutex` 串行化 + 代际 id（并发 POST：后来者 409 busy）。

```
POST {base}/_system/config
 ① 归一：load_cfg（load_with_extra 全管线：解密→类型→mounts→legacy 键）
    → fold_cli_mounts 重放 → cert/key 覆盖重放 → admission_gate
    归一后 cfg 的 sha256 与当前代相同 → 200 {"msg":"unchanged"}（幂等）
    ⚠ 幂等比较必须基于归一后 cfg，不能比磁盘原文（否则 CLI 覆盖语义被误判）
    静态验证失败 → 400 + 具体原因，旧世界继续
 ② 影子装配（专用 std::thread + 独立 runtime block_on，prewarm_boot 样板；
    from_config 含同步 join 段 app.rs:96-108 / tasks.rs:231-262，禁止在
    axum handler 里直接 await）
    - 迁移强制 verify-only，seed 不重放（dev 默认 auto-apply 会打生产库，
      app.rs:1272-1307）
    - 复用旧 Registries（from_config 增可选参数，跳过 assemble_plugins，
      app.rs:923）——插件不二次 init（oj-auth OnceLock GUARD keep-first
      已证实全局态重入不安全，plugins/oj-auth/src/lib.rs:75）
    - **第二轮复核新增（关键）**：影子装配必须复用旧 `StableState`
      （`app.rs:1249` `stable = backend.stable().clone()`）中的**全部后端连接**
      （db/redis/blob/es/broker/mail/ldap/auth/smtp 的 pool），**绝不按新 cfg
      重新建池**。`reuse_registries` 跳过 `assemble_plugins` 后，后端实例由插件
      持有且不复初始化 → 旧 pool 自动沿用；新 cfg 中 `restart_pending` 段的
      值只记录、不接线（见 §6）。若 `assemble_backend` 在 `reuse_registries`
      下仍新建 pool，则（a）`restart_pending`「沿旧」被打破、（b）重载瞬间闪建
      连接——实现必须规避。
    - mounts 变化（含新增 prefix）：更新 ReloadState.prefixes 集合（§3 修订），
      `{base}/_system/*` 立即对新 prefix 生效，无需重建外层 Router。
    失败 → 400 + 具体原因，旧世界继续
 ③ 任务池交接（顺序是回滚完整性的关键，swap 必须最后）
    a. 旧 tasks_flag 显式 store(true)（信号处理器不会替你置，
       oj/src/tasks.rs:140）
    b. spawn_blocking 旧池 shutdown_and_join（阻塞调用，
       serve_cmd.rs:161-168 先例）
    c. 装配新任务池（crontab 解析/scan 超 max/probe panic 等 fail-fast
       路径，tasks.rs:289-311）
    c 失败 → 按旧 cfg 重装旧任务池 + 返回 500（HTTP 未切，回滚完整）
 ④ watch swap：新 Router send 进 watch → 新请求瞬时走新世界 →
    旧 App 随在途请求完成自然 drop → 200
    {code:0, data:{generation, applied:[...], restart_pending:[...],
                   kept_listener:[...]}}
```

**cron 语义**：不双跑（旧驱动 flag 置位即退、新 registry 从装配时刻起算 next_run），但交接窗口内 tick **丢拍不补**（`tasks.rs:377-380`、`task_pool.rs:210-211`）——文档红线。

**幂等细节**：磁盘内容相同但 mtime 变（touch）→ sha256 同 → unchanged，正确。

## 6. v1 适用矩阵（「绝无静默不生效」的落地）

POST 返回体三清单：`applied`（本代已生效）/ `restart_pending`（已验证合法但本代沿用旧值，重启后生效）/ `kept_listener`（listener 级沿旧 + warn）。

| 即时生效（applied） | restart_pending / kept_listener |
|---|---|
| mounts / JS 模块树、vars、db_query、db_trace、OIDC、CORS、tasks、ws、tenant、http_sso | db、redis、blob、es、broker、smtp、ldap、auth、kafkas、rabbits、plugins、plugins_dir、secrets（插件轴与凭据，复用旧连接）；port/host/certificate_path/public_key_path（kept_listener，**沿旧 + warn**）；logs_dir/logs_max_m/logs_keep_files（日志只在 run() re-init，`serve_cmd.rs:113`） |

清单实现 = 新旧 Config 顶层段逐段 diff（`serde_json::Value` 相等比较，已知键表 `known_top_level_keys()` 复用）。逐段归属以实现计划期对账为准，方向：插件轴 vtable 消费的段 → restart_pending；from_config 直接消费进新 Router/StableState 的段 → applied。

## 7. 必改清单（实现计划入口）

0. **新增 `server.reload_api` 配置字段**：`ServerCfg`（`src/config.rs:109`）增
   `pub reload_api: bool`，`#[serde(default)]`；`Default` 于 `:198` 置 `false`。
   （第二轮复核发现：原设计全程引用该字段但 §7 漏列，且全仓此前无此字段。）
1. `oj/src/serve_cmd.rs:71-76`：`a.cert_path`/`a.key_path` 是 **move**，改 `.clone()`——否则 CLI 覆盖无法重放（编译不过）。
2. `App::from_config` 增可选 `reuse_registries` 参数（None = 现状零变化；Some = 跳过 assemble_plugins/复用 PluginInfo），并据 §5 ② 在 `Some` 下复用旧 `StableState` 后端连接、不重建 pool。
3. `serve_graceful` → 新增 swappable 服务形态：listener 永生 + 外层 Router（单一 fallback，持有 `Arc<ReloadState>`：watch Sender、Mutex、代际 id、归一 cfg 快照、config 路径、auth_guard、**已知 mount prefix 集合**）。外层 fallback 分流 `_system`/health（§3 修订）后把其余请求 `oneshot` 给当前内层 Router。现有 `serve`/`dispatch` 路径不动。
   - **第二轮复核新增**：`serve_graceful` 必须用 `into_make_service_with_connect_info(SocketAddr)` 注入连接信息，供 `_system/config` 回环守卫提取（§4）。
4. `serve/src/lib.rs:229-232`：health 注册从 serve::app 内迁至外层 fallback（新位 `{p}/_system/health` + 旧别名 `{p}/health` warn）；plugins 注册原地不动（随内层每代重建）。
5. `serve/src/certificate_watcher.rs:48-109`：detached `thread::park()` 每次装配泄漏一条线程 + inotify watch → **提为进程级单例**（按 resolved 路径去重，不重复 spawn；`app.rs:789` 在 `from_config` 内，重载路径极易再次触发，单例必须覆盖每代复用）。
6. 迁移门禁增「热更路径强制 verify-only」参数位（from_config 调用方传入，启动路径不变）。

## 8. 双专家复核结论存档（2026-10-12）

两位独立复核总裁决一致：**永生 listener + 外层静态 Router + watch 切内层与代码库事实兼容，可进实现**。放行前置四条已全部落入本设计：

- 任务池先收后建、swap 最后（§5 ③）
- `_system` 端点自带守卫（§4；route() 直注册会绕过 AuthGuard 管线，`lib.rs:735` admit 只在 fallback 腿被调——因此 `_system/config` 必须挂外层但**自拦鉴权**，不得照抄 health 的匿名注册）
- v1 插件不重装（§5 ②）
- 外层 fallback 的 WS 升级补真 TCP e2e（§10）

未采纳/降级项：
- 专家 B 提议 `restart_pending` 分段应用（其余段照常 + 列表）——采纳；专家 A 原提「plugins 段变化即整次拒绝 400」——降级为 §6 三清单语义（更少拆单提交）。
- GET redact：GET 摘要不回显内容，redact 需求消解。
- POST body 尺寸上限：POST 无 body，非空即拒，消解。
- WS_LIVE 进程级 static（`serve/src/ws.rs:22`）：旧世界 WS 连接占用新代连接预算，热更后新升级可能瞬时 503——v1 注明可接受，per-generation 闸门留待需要时做。

## 9. 边界与文档红线（进 devkit 文档）

1. 切换窗口内新旧世界并存（vars 读到旧值的最后一批在途请求）——无跨请求原子性。
2. cron 丢拍不补（§5）；窗口内旧 `{base}/tasks` API run-once 可能被拒（旧池 stopping）——错误信封如实返回。
3. 旧世界被 WS 长连接钉死：旧 Backend（db 池等）存活至最后一条旧 WS 连接关闭（`serve/src/ws.rs:195-201` RoutePool 持 bridge 工厂）——反复热更 N 代并存；`WsOptions.idle_linger_ms` 空闲断连可缓解，文档写明内存上界语义。
4. api prefix 变更不迁移 `_system`/health 的注册 base（§3）。
5. 证书过期期间：业务 GET 走内层门禁 403，`_system` 在外层 fallback 不受限（运维可用）。
6. 新增 api 挂载前缀：`{base}/_system/*` 与 health 经 ReloadState.prefixes 集合自动覆盖新 prefix（§3 修订），无需重启；该 prefix 的业务路由随内层 swap 生效。

## 10. 测试与验收

e2e（`oj/tests/e2e.rs` 惯例，真实进程 + 真 TCP）：

1. **核心链路**：起 serve（temp dir，reload_api 开）→ 改 config.yaml（vars 加键）→ POST → 200 → dispatch 业务端点断言新值生效；GET 摘要 generation 递增。
2. **验证失败保持旧世界**：写坏 YAML（类型错 / 挂载不存在 / ENC 无私钥）→ POST 400 + 原因 → 旧端点仍服务旧值。
3. **幂等**：不改内容再 POST → "unchanged"，generation 不变。
4. **restart_pending / kept_listener**：改 db DSN（不可达新 DSN 也应 restart_pending 而非 400，因不真连）→ 200 + restart_pending 含 "db"；改 port → 200 + kept_listener 含 "port" + 旧端口仍服务。
5. **回滚完整性**：改 tasks 段 crontab 为非法 → 500 → 旧任务池仍按旧 cfg 运行 → 业务端点不受影响。
6. **真 TCP WS 升级 e2e**：真 TCP 客户端 → 外层 fallback → 内层 ws 路由 → 101 升级 + 收发一帧（现有 oneshot 测试路径覆盖不到，验收硬门槛）。
7. **守卫**：reload_api 缺省 → `_system/config` 404；开启 + auth 配置 → 无 Bearer 401；开启 + auth 未配 → 非回环 403。
8. **health 迁移**：新位 200；旧位 200 + warn 日志。
9. **并发互斥**：双 POST 竞跑 → 恰一个 200、另一个 409 busy；generation 恰好 +1。

单测：段 diff 三清单分类、归一 sha256 幂等、`_system` 保留路径冲突 warn。

## 11. 版本纪律（红线）

- CHANGELOG.md 条目（health URL 迁移标注 BREAKING、reload_api、`_system` 命名空间）。
- `docs/devkit/` 四件套对齐：api-manual（新端点 + 错误表 + restart_pending 语义）、scenarios（热更场景）、SKILL.md（陷阱：幂等基于归一 cfg、cron 丢拍、WS 钉死）、README。
- 改完 `cargo xtask build` 归置 `bin/devkit/`，devkit 契约用例校验。

## 12. 第二轮复核修订（2026-10-12）

复核方逐项核对了设计锚点与代码库事实，修订如下：

1. **§7 补 `server.reload_api` 字段**：全仓此前无此字段，必改清单漏列，已补为项 0（config.rs:109 + Default:198）。
2. **§3 架构修订**：原「外层逐 prefix 注册 `_system` 具体路由」与「外层静态」冲突，且重载新增 prefix 会丢 `_system` 端点（违反绝无静默不生效）。改为**外层单一 fallback 在 ReloadState 已知 prefix 集合上分流处理 `_system`/health**，外层保持构造一次、不挂中间件；mounts 变化只更新 prefix 集合。同时仍满足 §9.5 证书到期运维可用。
3. **§4 回环守卫依赖**：`ConnectInfo<SocketAddr>` 须由 serve 栈注入，`serve_graceful` 须 `into_make_service_with_connect_info(SocketAddr)`，已写入 §7.3。
4. **§3 WS 引文更正**：`ws.rs:495` 实为单测内部取值，非 OnUpgrade 要求；WS 透传属 axum 内部语义，已重写表述。
5. **§5 ② 复用旧后端连接**：明确 `reuse_registries` 下 `assemble_backend` 不得按新 cfg 重建 pool（否则 restart_pending「沿旧」被破 + 重载闪建连接），新增为关键约束。
6. **§7.5 证书 watcher 去重**：按 resolved 路径去重、每代复用同一单例，避免反复重载累积线程/inotify fd。
7. **§9 新增边界 6**：新增挂载 prefix 的 `_system`/health 自动覆盖，无需重启。

未改动的双专家结论（§8）与四条放行前置仍然成立。
