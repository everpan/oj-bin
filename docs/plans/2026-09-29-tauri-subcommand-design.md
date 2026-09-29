# `oj tauri` 子命令设计（桌面端无端口集成，Mode C）

- 日期：2026-09-29
- 状态：v2（已据架构/工程/产品三路评审调整；处置见 §12）
- 关联调研：`docs/tauri-integration.md`；本设计采纳「模式 C：JSON/REST 无端口，WS 混合监听」

## 1. 背景与目标

oj 是嵌入 V8 的低代码后端框架，HTTP 服务本质是一个标准 `axum::Router`
（`server/src/lib.rs:174` `app()`；`oj::app::App` 暴露 `from_config` 与 `dispatch(req)`
经 `router.oneshot` 在进程内完成路由 + 前置管线 + 鉴权 + 证书门禁，`oj/src/app.rs:1195` /
`server/src/lib.rs:354` `handle`）。

目标：新增 `oj tauri` 子命令，把 oj 后端以**桌面应用**形态交付。让前端（Tauri webview）
调用 oj handler 时**本地不起面向业务的 TCP 端口**——经 Tauri IPC / 自定义协议在进程内
`App::dispatch` 处理。

**价值重述（评审修正）**：`127.0.0.1` 本就不暴露局域网，故「无端口」的安全收益有限；
真正的价值是**单二进制桌面分发** + **前后端同进程、共享内存、统一生命周期**，以及避免
`oj serve` 的端口/生命周期管理。无端口是这一形态的伴随结果，而非首要卖点。

## 2. 范围与边界（YAGNI）

- **做**：`oj tauri` 脚手架生成 + 开发/构建承载；桌面运行时以「IPC/自定义协议桥接」跑通
  oj 的 `/v1/api/*` 与 `/blob/*`（无端口）；sqlite 本地库开箱可用；WS 走本机最小监听。
- **不做**：不重写 oj 运行时；不替换 Tauri 前端构建；不内嵌 mysql/postgres/redis/s3
  等需插件的后端（桌面默认 sqlite，可选随包分发插件 cdylib）。
- **迁移注意**：依赖 HTTP 语义（multipart 上传、证书 GET 门禁、WS）的 handler 在桌面形态下
  行为一致（同一 `handle`/`pipeline`），无业务代码迁移清单之外的漂移。

## 3. 总体架构（采纳：生成器 + 伴生运行时 crate）

oj 主 CLI **不链 Tauri**，保持轻 CLI 定位；Tauri 依赖隔离在独立 crate `oj-tauri`。

```
oj/ (主 CLI)
  └─ args.rs: Commands::Tauri { #[command(subcommand)] cmd: TauriCmd }
       TauriCmd::New { name, frontend }   生成 Tauri 工程骨架 + 写 oj-tauri 依赖
       TauriCmd::Dev                      调起 `cargo tauri dev`（生成工程内）
       TauriCmd::Build                    调起 `cargo tauri build`
  └─ tauri_cmd.rs: 仅生成代码 / 透传 cargo tauri，不跑 webview

oj-tauri/ (伴生 crate，依赖 oj + tauri)
  └─ main.rs:
       - setup 中 `oj::app::App::from_config(cfg, ...).await?` 构造 App（同 oj serve 装配）
       - `tauri::Builder::manage(Arc<App>)` 注入状态
       - 注册 `#[tauri::command] oj_dispatch(method, uri, headers, body) -> OjResp`
           内部 `APP.dispatch(req).await`，Response 转 OjResp{status,headers,body}
       - 注册**自定义协议**（如 `oj://` 或 `http://oj.local`）：协议处理器内部调
         `oj_dispatch`，使前端 `fetch` **零改动**
       - WS：另起最小 axum 监听（见 §7），端口经 invoke 传给前端
       - 窗口关闭 → 置 `App::tasks_flag()` 触发优雅停机
  └─ 不调用 serve_graceful（业务 API 无监听）；仅 App::dispatch + WS 监听
```

## 4. 数据流（零改 fetch）

```
webview 业务代码（原样 fetch）
  → fetch('oj://v1/api/user/account/?id=1')        // 自定义协议伪装同源
  → Tauri 协议处理器 → oj_dispatch(...)
  → Rust: Request::builder().method(uri).headers(body)
        → APP.dispatch(req).await                   // 完整 router + pipeline + auth + cert 门禁
        → OjResp{status, headers, body_bytes}
  → 协议处理器回写 Response → fetch 解析 envelope {code,msg,data}
```

`App::dispatch` 已覆盖静态站点兜底、`/blob/*` GET（含 Range）、证书 GET 门禁、auth/tenant
前置管线（`server/src/lib.rs:354` `handle`）。**业务代码无需把 `fetch` 换成 `ojFetch`**——
自定义协议在传输层完成 IPC 桥接（评审采纳产品意见，避免破坏性改造）。

## 5. 认证模型（关键）

`dispatch` 走完整前置管线，oj-auth Bearer 守卫照常生效。桌面端推荐落地：

- **Tauri 后端作可信代理**：`oj tauri` 脚手架期用 oj-auth 的密钥预签发/生成「本地服务账号」
  keypair 并随包分发证书；`oj_dispatch` 在构造 Request 时自动注入
  `Authorization: Bearer <local-jwt>`。前端无需管理令牌；webview 已在进程内、视为可信。
  保留 oj 既有鉴权语义，零改造 handler。

**硬性约束（评审修正，删除 `--no-cert`）**：`from_config` 强制要求
`server.public_key_path` + `server.certificate_path` 必配，否则 fail-fast，且**代码明言无任何
配置/旗标可跳过**（`oj/src/app.rs:951`）。故桌面端**必须随包分发证书**——脚手架期生成
自签名 keypair 写入 config 指向的资源路径，不做「跳过证书」后门（不破坏证书强制校验不变量）。
证书续期：桌面场景比服务端更痛，列为 §11 开放问题（随包证书 + 版本升级时重签）。

## 6. 配置 / 模块 / 插件打包

- `config.yaml` + `dist`（release，不转译、按锁聚合）作为 Tauri 资源随包；`oj tauri build`
  先 `oj build`（src→dist）再随包。
- 本地库：`db.default` 指向用户目录下的 sqlite 文件（内置，无需插件）。
- 插件 cdylib（可选）：**显式**设 `OJ_PLUGINS_DIR` 或 config `plugins_dir` 指向
  `tauri::Manager::resolve_resource` 解压目录（不可依赖 `<exe>/plugins` 默认，资源在
  `Resources` 目录）；`oj tauri build` 把 `bin/plugins/<host-triple>/*` 拷入资源并写入该变量。
- `App::from_config` 的 `config_dir` 指向资源解压目录（Tauri `resolveResource`）。
- 插件须与 `ABI_VERSION`（`oj-plugin-ffi`）严格相等且同 `host-triple` 构建，否则加载即
  fail-fast；`oj tauri build` 显式校验 cert + plugins 存在再打包。

## 7. WebSocket（评审定稿：混合，非 spike）

`App::dispatch` 经 `router.oneshot` 派发——**`oneshot` 下 WS 升级握手虽返回 101，但永不跑
帧循环**（`app.rs:6-8` 明示；`on_upgrade` 闭包依赖真实 socket，oneshot 无 TCP 连接故不触发）。
故**无端口直连 WS 在架构上不可能**，原「待 spike 验证」为误判，直接定方案：

- **JSON / REST / 静态 / blob：无端口**，走 `App::dispatch`（已被 `oj test` 的
  `op_client_dispatch`→`App::dispatch` 实证，可用，无需 spike）。
- **WS：复用 `App::ws_router()` + 本机最小 axum 监听**（127.0.0.1 随机端口；`client.ws`
  已有同构实现，`app.rs:1301` 注释），前端经 `invoke('oj_ws_port')` 拿到端口后直连
  `ws://127.0.0.1:<port>/...`。该监听仅服务 WS 路由，不暴露业务 REST（REST 仍走 dispatch），
  且不暴露局域网（bind 127.0.0.1）。

## 8. 生命周期与并发

- `App` 在 Tauri `setup` 构造一次，`Arc` 注入 `manage`。**`App: Send + Sync + 'static` 已由
  `impl ClientTransport` 编译证明**（`app.rs:49-55`，并被 `oj test` 注入 OpState），`manage`
  可直接 `Arc<App>`，无需额外验证（评审修正，原「需编译验证」删除）。
- `Bridge` 为 `!Send`、跑 current_thread 池，oj 已内部封装；`dispatch` 从 Tauri 的
  multi_thread 调用安全（actor 自行 spawn 到其池）。oj serve 本就在 multi_thread 跑，故
  runtime 共存风险低；集成时以 `manage(Arc::new(app))` 编过即为最终确认。
- 停机：窗口关闭 → `App::tasks_flag().store(true)` → 在途 `dispatch` 自然结束；WS 监听
  `abort` 对应 JoinHandle。

## 9. 实施分阶段（评审重排：先 demo 验证价值）

0. **Sample 桌面 demo（先行验收物）**：在 `sample/`（或 `sample-desktop/`）手写一个最小
   Tauri 工程，直接依赖 `oj` + `tauri`，跑通「webview fetch → 自定义协议 → App::dispatch →
   信封」与「sqlite 本地库」。先证明桌面形态有价值，再投完整子命令。
1. **`oj tauri new` 脚手架**：生成 `TauriCmd::New` 工程骨架（含 `oj-tauri` 依赖、自签名
   证书、config 模板），`dev`/`build` 透传 `cargo tauri`。
2. **`oj-tauri` 运行时**：`from_config` + `oj_dispatch` 桥接（JSON/REST 先行）。
3. **零改 fetch**：自定义协议处理器包装 `oj_dispatch`（取代 `ojFetch` 方案）。
4. **`oj tauri build` 串联**：`oj build` + 资源/证书/插件随包 + cert/plugins 存在性 fail-fast。
5. **WS 混合监听**：`App::ws_router()` + 127.0.0.1 随机端口 + `oj_ws_port` invoke。

## 10. 验收标准

- Sample 桌面 demo：`webview` 内 `fetch('oj://v1/api/...')` 调 oj handler 返回正确信封，
  **业务 REST 无监听端口**（`lsof` 仅见 WS 的 127.0.0.1 随机端口）。
- 证书/鉴权：经 `oj_dispatch` 自动注入 Bearer，handler 内 `auth` 模块正常取到身份；无
  `--no-cert` 后门。
- 静态站点 / `/blob/*` GET 经 `dispatch` 可达；`/blob` 大文件响应体**设内存上限**（见 §11）。
- WS：经混合监听可订阅广播。
- `oj tauri new demo && cd demo && cargo tauri dev` 端到端跑通。

## 11. 开放问题

- Q1（已决）：证书强制随包（脚手架生成自签名 keypair），不做 `--no-cert`。
- Q2：前端桥默认用 Tauri 自定义协议（零改 `fetch`）还是仍提供 `@oj/tauri-fetch` npm 包？
  倾向前者。
- Q3：多窗口下 `App` 状态共享与长任务隔离如何约定？
- Q4：WS 随机端口经 `invoke` 传给前端后，如何防本机其他进程连入（bind 127.0.0.1 + 一次性
  token？）。
- Q5：`/blob` 大文件经 `dispatch` 全量读入 `OjResp.body` 有 OOM 风险，是否设上限或改前端
  直连 WS/blob 专用监听？
- Q6：桌面证书过期/续期如何随 oj 版本升级接力（无 `oj-cert renew` 静默通道）。

## 12. 三路评审处置（架构 / 工程 / 产品）

| # | 来源 | 意见 | 处置 |
|---|------|------|------|
| 1 | 工程【严重】 | `oneshot` WS 不可行（app.rs:6-8） | **采纳**：§7 直接定混合监听，删 WS spike |
| 2 | 工程/架构【认可】 | `App` Send+Sync 已编译实证 | **采纳**：§8 删「需编译验证」 |
| 3 | 架构【严重】 | `--no-cert` 违反证书硬门禁 | **采纳**：§5 删 `--no-cert`，强制随包证书 |
| 4 | 产品【严重】 | `ojFetch` 破坏性改造 | **采纳**：§3/§4 改自定义协议零改 `fetch` |
| 5 | 产品【严重】 | 无端口安全收益被高估、缺场景证据 | **采纳并重述**：§1 把价值改为单二进制分发+同进程；保留无端口为伴随结果 |
| 6 | 产品【严重】 | DX 割裂、迁移阻力大 | **部分采纳**：§2 补迁移说明；未做完整迁移清单（YAGNI，demo 阶段补） |
| 7 | 产品【建议】 | 先 demo 后子命令 | **采纳**：§9 重排 Phase 0 sample demo 先行 |
| 8 | 架构【建议】 | 插件发现须显式设 `OJ_PLUGINS_DIR`→resolveResource | **采纳**：§6 显式化 |
| 9 | 架构【建议】 | WS 混合监听复用 `App::ws_router()`+本地 bind | **采纳**：§7 |
| 10 | 工程【建议】 | `/blob` 大文件响应体驻内存 | **采纳为开放问题**：§11 Q5 |
| 11 | 架构【建议】 | 可先仅做 `new` 脚手架 | **采纳**：§9 Phase 1 仅 `new` |
| 12 | 工程【认可】 | `dispatch` 已被 oj test 实证，spike(a) 冗余 | **采纳**：删 REST spike |
